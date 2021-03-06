use crate::avm1::listeners::SystemListener;
use crate::avm1::Avm1;
use crate::backend::input::InputBackend;
use crate::backend::{
    audio::AudioBackend, navigator::NavigatorBackend, render::Letterbox, render::RenderBackend,
};
use crate::context::{ActionQueue, ActionType, RenderContext, UpdateContext};
use crate::display_object::{MorphShape, MovieClip};
use crate::events::{ButtonEvent, ButtonKeyCode, ClipEvent, PlayerEvent};
use crate::library::Library;
use crate::prelude::*;
use crate::transform::TransformStack;
use gc_arena::{make_arena, ArenaParameters, Collect, GcCell};
use log::info;
use rand::{rngs::SmallRng, SeedableRng};
use std::convert::TryFrom;
use std::sync::Arc;

static DEVICE_FONT_TAG: &[u8] = include_bytes!("../assets/noto-sans-definefont3.bin");

/// The newest known Flash Player version, serves as a default to
/// `player_version`.
pub const NEWEST_PLAYER_VERSION: u8 = 32;

#[derive(Collect)]
#[collect(no_drop)]
struct GcRoot<'gc>(GcCell<'gc, GcRootData<'gc>>);

#[derive(Collect)]
#[collect(no_drop)]
struct GcRootData<'gc> {
    library: Library<'gc>,
    root: DisplayObject<'gc>,
    mouse_hovered_object: Option<DisplayObject<'gc>>, // TODO: Remove GcCell wrapped inside GcCell.

    /// The object being dragged via a `startDrag` action.
    drag_object: Option<DragObject<'gc>>,

    avm: Avm1<'gc>,
    action_queue: ActionQueue<'gc>,
}

impl<'gc> GcRootData<'gc> {
    /// Splits out parameters for creating an `UpdateContext`
    /// (because we can borrow fields of `self` independently)
    fn update_context_params(
        &mut self,
    ) -> (
        DisplayObject<'gc>,
        &mut Library<'gc>,
        &mut ActionQueue<'gc>,
        &mut Avm1<'gc>,
        &mut Option<DragObject<'gc>>,
    ) {
        (
            self.root,
            &mut self.library,
            &mut self.action_queue,
            &mut self.avm,
            &mut self.drag_object,
        )
    }
}
type Error = Box<dyn std::error::Error>;

make_arena!(GcArena, GcRoot);

pub struct Player<
    Audio: AudioBackend,
    Renderer: RenderBackend,
    Navigator: NavigatorBackend,
    Input: InputBackend,
> {
    /// The version of the player we're emulating.
    ///
    /// This serves a few purposes, primarily for compatibility:
    ///
    /// * ActionScript can query the player version, ostensibly for graceful
    ///   degradation on older platforms. Certain SWF files broke with the
    ///   release of Flash Player 10 because the version string contains two
    ///   digits. This allows the user to play those old files.
    /// * Player-specific behavior that was not properly versioned in Flash
    ///   Player can be enabled by setting a particular player version.
    player_version: u8,

    swf_data: Arc<Vec<u8>>,
    swf_version: u8,

    is_playing: bool,

    audio: Audio,
    renderer: Renderer,
    navigator: Navigator,
    input: Input,
    transform_stack: TransformStack,
    view_matrix: Matrix,
    inverse_view_matrix: Matrix,

    rng: SmallRng,

    gc_arena: GcArena,
    background_color: Color,

    frame_rate: f64,
    frame_accumulator: f64,
    global_time: u64,

    viewport_width: u32,
    viewport_height: u32,
    movie_width: u32,
    movie_height: u32,
    letterbox: Letterbox,

    mouse_pos: (Twips, Twips),
    is_mouse_down: bool,
}

impl<
        Audio: AudioBackend,
        Renderer: RenderBackend,
        Navigator: NavigatorBackend,
        Input: InputBackend,
    > Player<Audio, Renderer, Navigator, Input>
{
    pub fn new(
        mut renderer: Renderer,
        audio: Audio,
        navigator: Navigator,
        input: Input,
        swf_data: Vec<u8>,
    ) -> Result<Self, Error> {
        let swf_stream = swf::read::read_swf_header(&swf_data[..]).unwrap();
        let header = swf_stream.header;
        let mut reader = swf_stream.reader;

        // Decompress the entire SWF in memory.
        // Sometimes SWFs will have an incorrectly compressed stream,
        // but will otherwise decompress fine up to the End tag.
        // So just warn on this case and try to continue gracefully.
        let data = if header.compression == swf::Compression::Lzma {
            // TODO: The LZMA decoder is still funky.
            // It always errors, and doesn't return all the data if you use read_to_end,
            // but read_exact at least returns the data... why?
            // Does the decoder need to be flushed somehow?
            let mut data = vec![0u8; swf_stream.uncompressed_length];
            let _ = reader.get_mut().read_exact(&mut data);
            data
        } else {
            let mut data = Vec::with_capacity(swf_stream.uncompressed_length);
            if let Err(e) = reader.get_mut().read_to_end(&mut data) {
                log::error!("Error decompressing SWF, may be corrupt: {}", e);
            }
            data
        };

        let swf_len = data.len();

        info!("{}x{}", header.stage_size.x_max, header.stage_size.y_max);

        let movie_width = (header.stage_size.x_max - header.stage_size.x_min).to_pixels() as u32;
        let movie_height = (header.stage_size.y_max - header.stage_size.y_min).to_pixels() as u32;

        let mut player = Player {
            player_version: NEWEST_PLAYER_VERSION,

            swf_data: Arc::new(data),
            swf_version: header.version,

            is_playing: false,

            background_color: Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
            transform_stack: TransformStack::new(),
            view_matrix: Default::default(),
            inverse_view_matrix: Default::default(),

            rng: SmallRng::from_seed([0u8; 16]), // TODO(Herschel): Get a proper seed on all platforms.

            gc_arena: GcArena::new(ArenaParameters::default(), |gc_context| {
                // Load and parse the device font.
                let device_font =
                    match Self::load_device_font(gc_context, DEVICE_FONT_TAG, &mut renderer) {
                        Ok(font) => Some(font),
                        Err(e) => {
                            log::error!("Unable to load device font: {}", e);
                            None
                        }
                    };

                let mut library = Library::new();
                library.set_device_font(device_font);
                GcRoot(GcCell::allocate(
                    gc_context,
                    GcRootData {
                        library,
                        root: MovieClip::new_with_data(
                            header.version,
                            gc_context,
                            0,
                            0,
                            swf_len,
                            header.num_frames,
                        )
                        .into(),
                        mouse_hovered_object: None,
                        drag_object: None,
                        avm: Avm1::new(gc_context, NEWEST_PLAYER_VERSION),
                        action_queue: ActionQueue::new(),
                    },
                ))
            }),

            frame_rate: header.frame_rate.into(),
            frame_accumulator: 0.0,
            global_time: 0,

            movie_width,
            movie_height,
            viewport_width: movie_width,
            viewport_height: movie_height,
            letterbox: Letterbox::None,

            mouse_pos: (Twips::new(0), Twips::new(0)),
            is_mouse_down: false,

            renderer,
            audio,
            navigator,
            input,
        };

        player.gc_arena.mutate(|gc_context, gc_root| {
            let root_data = gc_root.0.write(gc_context);
            let mut root = root_data.root;
            root.post_instantiation(gc_context, root, root_data.avm.prototypes().movie_clip);
        });

        player.build_matrices();
        player.preload();

        Ok(player)
    }

    pub fn tick(&mut self, dt: f64) {
        // Don't run until preloading is complete.
        // TODO: Eventually we want to stream content similar to the Flash player.
        if !self.audio.is_loading_complete() {
            return;
        }

        if self.is_playing() {
            self.frame_accumulator += dt;
            self.global_time += dt as u64;
            let frame_time = 1000.0 / self.frame_rate;

            let needs_render = self.frame_accumulator >= frame_time;

            const MAX_FRAMES_PER_TICK: u32 = 5; // Sanity cap on frame tick.
            let mut frame = 0;
            while frame < MAX_FRAMES_PER_TICK && self.frame_accumulator >= frame_time {
                self.frame_accumulator -= frame_time;
                self.run_frame();
                frame += 1;
            }

            // Sanity: If we had too many frames to tick, just reset the accumulator
            // to prevent running at turbo speed.
            if self.frame_accumulator >= frame_time {
                self.frame_accumulator = 0.0;
            }

            if needs_render {
                self.render();
            }

            self.audio.tick();
        }
    }

    /// Returns the approximate duration of time until the next frame is due to run.
    /// This is only an approximation to be used for sleep durations.
    pub fn time_til_next_frame(&self) -> std::time::Duration {
        let frame_time = 1000.0 / self.frame_rate;
        let dt = if self.frame_accumulator <= 0.0 {
            frame_time
        } else if self.frame_accumulator >= frame_time {
            0.0
        } else {
            frame_time - self.frame_accumulator
        };
        std::time::Duration::from_micros(dt as u64 * 1000)
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    pub fn set_is_playing(&mut self, v: bool) {
        if v {
            // Allow auto-play after user gesture for web backends.
            self.audio.prime_audio();
        }
        self.is_playing = v;
    }

    pub fn movie_width(&self) -> u32 {
        self.movie_width
    }

    pub fn movie_height(&self) -> u32 {
        self.movie_height
    }

    pub fn viewport_dimensions(&self) -> (u32, u32) {
        (self.viewport_width, self.viewport_height)
    }

    pub fn set_viewport_dimensions(&mut self, width: u32, height: u32) {
        self.viewport_width = width;
        self.viewport_height = height;
        self.build_matrices();
    }

    pub fn handle_event(&mut self, event: PlayerEvent) {
        let mut needs_render = false;

        // Update mouse position from mouse events.
        if let PlayerEvent::MouseMove { x, y }
        | PlayerEvent::MouseDown { x, y }
        | PlayerEvent::MouseUp { x, y } = event
        {
            self.mouse_pos =
                self.inverse_view_matrix * (Twips::from_pixels(x), Twips::from_pixels(y));
            if self.update_roll_over() {
                needs_render = true;
            }
        }

        // Propagate button events.
        let button_event = match event {
            // ASCII characters convert directly to keyPress button events.
            PlayerEvent::TextInput { codepoint }
                if codepoint as u32 >= 32 && codepoint as u32 <= 126 =>
            {
                Some(ButtonEvent::KeyPress {
                    key_code: ButtonKeyCode::try_from(codepoint as u8).unwrap(),
                })
            }

            // Special keys have custom values for keyPress.
            PlayerEvent::KeyDown { key_code } => {
                if let Some(key_code) = crate::events::key_code_to_button_key_code(key_code) {
                    Some(ButtonEvent::KeyPress { key_code })
                } else {
                    None
                }
            }
            _ => None,
        };

        if button_event.is_some() {
            self.mutate_with_update_context(|_avm, context| {
                let root = context.root;
                if let Some(button_event) = button_event {
                    root.propagate_button_event(context, button_event);
                }
            });
        }

        // Propagte clip events.
        let (clip_event, mouse_event_name) = match event {
            PlayerEvent::KeyDown { .. } => (Some(ClipEvent::KeyDown), Some("onKeyDown")),
            PlayerEvent::MouseMove { .. } => (Some(ClipEvent::MouseMove), Some("onMouseMove")),
            PlayerEvent::MouseUp { .. } => (Some(ClipEvent::MouseUp), Some("onMouseUp")),
            PlayerEvent::MouseDown { .. } => (Some(ClipEvent::MouseDown), Some("onMouseDown")),
            _ => (None, None),
        };

        if clip_event.is_some() || mouse_event_name.is_some() {
            self.mutate_with_update_context(|_avm, context| {
                let root = context.root;

                if let Some(clip_event) = clip_event {
                    root.propagate_clip_event(context, clip_event);
                }

                if let Some(mouse_event_name) = mouse_event_name {
                    context.action_queue.queue_actions(
                        root,
                        ActionType::NotifyListeners {
                            listener: SystemListener::Mouse,
                            method: mouse_event_name,
                            args: vec![],
                        },
                        false,
                    );
                }
            });
        }

        let mut is_mouse_down = self.is_mouse_down;
        self.mutate_with_update_context(|avm, context| {
            if let Some(node) = context.mouse_hovered_object {
                if let Some(mut button) = node.clone().as_button() {
                    match event {
                        PlayerEvent::MouseDown { .. } => {
                            is_mouse_down = true;
                            needs_render = true;
                            button.handle_button_event(context, ButtonEvent::Press);
                        }

                        PlayerEvent::MouseUp { .. } => {
                            is_mouse_down = false;
                            needs_render = true;
                            button.handle_button_event(context, ButtonEvent::Release);
                        }

                        _ => (),
                    }
                }
            }

            Self::run_actions(avm, context);
        });
        self.is_mouse_down = is_mouse_down;
        if needs_render {
            // Update display after mouse events.
            self.render();
        }
    }

    /// Update dragged object, if any.
    fn update_drag(&mut self) {
        let mouse_pos = self.mouse_pos;
        self.mutate_with_update_context(|_avm, context| {
            if let Some(drag_object) = &mut context.drag_object {
                if drag_object.display_object.removed() {
                    // Be sure to clear the drag if the object was removed.
                    *context.drag_object = None;
                } else {
                    let mut drag_point = (
                        mouse_pos.0 + drag_object.offset.0,
                        mouse_pos.1 + drag_object.offset.1,
                    );
                    if let Some(parent) = drag_object.display_object.parent() {
                        drag_point = parent.global_to_local(drag_point);
                    }
                    drag_point = drag_object.constraint.clamp(drag_point);
                    drag_object
                        .display_object
                        .set_x(context.gc_context, drag_point.0.to_pixels());
                    drag_object
                        .display_object
                        .set_y(context.gc_context, drag_point.1.to_pixels());
                }
            }
        });
    }

    fn update_roll_over(&mut self) -> bool {
        // TODO: While the mouse is down, maintain the hovered node.
        if self.is_mouse_down {
            return false;
        }
        let mouse_pos = self.mouse_pos;
        // Check hovered object.
        self.mutate_with_update_context(|avm, context| {
            let root = context.root;
            let new_hovered = root.mouse_pick(root, (mouse_pos.0, mouse_pos.1));
            let cur_hovered = context.mouse_hovered_object;
            if cur_hovered.map(|d| d.as_ptr()) != new_hovered.map(|d| d.as_ptr()) {
                // RollOut of previous node.
                if let Some(node) = cur_hovered {
                    if let Some(mut button) = node.as_button() {
                        button.handle_button_event(context, ButtonEvent::RollOut);
                    }
                }

                // RollOver on new node.
                if let Some(node) = new_hovered {
                    if let Some(mut button) = node.as_button() {
                        button.handle_button_event(context, ButtonEvent::RollOver);
                    }
                }

                context.mouse_hovered_object = new_hovered;

                Self::run_actions(avm, context);
                true
            } else {
                false
            }
        })
    }

    fn preload(&mut self) {
        self.mutate_with_update_context(|_avm, context| {
            let mut morph_shapes = fnv::FnvHashMap::default();
            let root = context.root;
            root.as_movie_clip()
                .unwrap()
                .preload(context, &mut morph_shapes);

            // Finalize morph shapes.
            for (id, static_data) in morph_shapes {
                let morph_shape = MorphShape::new(context.gc_context, static_data);
                context
                    .library
                    .register_character(id, crate::character::Character::MorphShape(morph_shape));
            }
        });
    }

    pub fn run_frame(&mut self) {
        self.mutate_with_update_context(|avm, context| {
            let mut root = context.root;
            root.run_frame(context);
            Self::run_actions(avm, context);
        });

        // Update mouse state (check for new hovered button, etc.)
        self.update_drag();
        self.update_roll_over();

        // GC
        self.gc_arena.collect_debt();
    }

    pub fn render(&mut self) {
        let view_bounds = BoundingBox {
            x_min: Twips::new(0),
            y_min: Twips::new(0),
            x_max: Twips::from_pixels(self.movie_width.into()),
            y_max: Twips::from_pixels(self.movie_height.into()),
            valid: true,
        };

        self.renderer.begin_frame();

        self.renderer.clear(self.background_color.clone());

        let (renderer, transform_stack) = (&mut self.renderer, &mut self.transform_stack);

        transform_stack.push(&crate::transform::Transform {
            matrix: self.view_matrix,
            ..Default::default()
        });
        self.gc_arena.mutate(|_gc_context, gc_root| {
            let root_data = gc_root.0.read();
            let mut render_context = RenderContext {
                renderer,
                library: &root_data.library,
                transform_stack,
                view_bounds,
                clip_depth_stack: vec![],
            };
            root_data.root.render(&mut render_context);
        });
        transform_stack.pop();

        if !self.is_playing() {
            self.renderer.draw_pause_overlay();
        }

        self.renderer.draw_letterbox(self.letterbox);
        self.renderer.end_frame();
    }

    pub fn audio(&self) -> &Audio {
        &self.audio
    }

    pub fn audio_mut(&mut self) -> &mut Audio {
        &mut self.audio
    }

    // The frame rate of the current movie in FPS.
    pub fn frame_rate(&self) -> f64 {
        self.frame_rate
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn input(&self) -> &Input {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut Input {
        &mut self.input
    }

    fn run_actions<'gc>(avm: &mut Avm1<'gc>, context: &mut UpdateContext<'_, 'gc, '_>) {
        while let Some(actions) = context.action_queue.pop() {
            // We don't run frame actions if the clip was removed after it queued the action.
            if !actions.is_unload && actions.clip.removed() {
                continue;
            }
            match actions.action_type {
                // DoAction/clip event code
                ActionType::Normal { bytecode } => {
                    avm.insert_stack_frame_for_action(
                        actions.clip,
                        context.swf_version,
                        bytecode,
                        context,
                    );
                }
                // DoInitAction code
                ActionType::Init { bytecode } => {
                    avm.insert_stack_frame_for_init_action(
                        actions.clip,
                        context.swf_version,
                        bytecode,
                        context,
                    );
                }

                // Event handler method call (e.g. onEnterFrame)
                ActionType::Method { name } => {
                    avm.insert_stack_frame_for_avm_function(
                        actions.clip,
                        context.swf_version,
                        context,
                        name,
                    );
                }

                // Event handler method call (e.g. onEnterFrame)
                ActionType::NotifyListeners {
                    listener,
                    method,
                    args,
                } => {
                    // A native function ends up resolving immediately,
                    // so this doesn't require any further execution.
                    avm.notify_system_listeners(
                        actions.clip,
                        context.swf_version,
                        context,
                        listener,
                        method,
                        &args,
                    );
                }
            }
            // Execute the stack frame (if any).
            let _ = avm.run_stack_till_empty(context);
        }
    }

    fn build_matrices(&mut self) {
        // Create  view matrix to scale stage into viewport area.
        let (movie_width, movie_height) = (self.movie_width as f32, self.movie_height as f32);
        let (viewport_width, viewport_height) =
            (self.viewport_width as f32, self.viewport_height as f32);
        let movie_aspect = movie_width / movie_height;
        let viewport_aspect = viewport_width / viewport_height;
        let (scale, margin_width, margin_height) = if viewport_aspect > movie_aspect {
            let scale = viewport_height / movie_height;
            (scale, (viewport_width - movie_width * scale) / 2.0, 0.0)
        } else {
            let scale = viewport_width / movie_width;
            (scale, 0.0, (viewport_height - movie_height * scale) / 2.0)
        };
        self.view_matrix = Matrix {
            a: scale,
            b: 0.0,
            c: 0.0,
            d: scale,
            tx: margin_width * 20.0,
            ty: margin_height * 20.0,
        };
        self.inverse_view_matrix = self.view_matrix;
        self.inverse_view_matrix.invert();

        // Calculate letterbox dimensions.
        // TODO: Letterbox should be an option; the original Flash Player defaults to showing content
        // in the extra margins.
        self.letterbox = if margin_width > 0.0 {
            Letterbox::Pillarbox(margin_width)
        } else if margin_height > 0.0 {
            Letterbox::Letterbox(margin_height)
        } else {
            Letterbox::None
        };
    }

    /// Runs the closure `f` with an `UpdateContext`.
    /// This takes cares of populating the `UpdateContext` struct, avoiding borrow issues.
    fn mutate_with_update_context<F, R>(&mut self, f: F) -> R
    where
        F: for<'a, 'gc> FnOnce(&mut Avm1<'gc>, &mut UpdateContext<'a, 'gc, '_>) -> R,
    {
        // We have to do this piecewise borrowing of fields before the closure to avoid
        // completely borrowing `self`.
        let (
            player_version,
            global_time,
            swf_data,
            swf_version,
            background_color,
            renderer,
            audio,
            navigator,
            input,
            rng,
            mouse_position,
            stage_width,
            stage_height,
        ) = (
            self.player_version,
            self.global_time,
            &mut self.swf_data,
            self.swf_version,
            &mut self.background_color,
            &mut self.renderer,
            &mut self.audio,
            &mut self.navigator,
            &mut self.input,
            &mut self.rng,
            &self.mouse_pos,
            Twips::from_pixels(self.movie_width.into()),
            Twips::from_pixels(self.movie_height.into()),
        );

        self.gc_arena.mutate(|gc_context, gc_root| {
            let mut root_data = gc_root.0.write(gc_context);
            let mouse_hovered_object = root_data.mouse_hovered_object;
            let (root, library, action_queue, avm, drag_object) = root_data.update_context_params();
            let mut update_context = UpdateContext {
                player_version,
                global_time,
                swf_data,
                swf_version,
                library,
                background_color,
                rng,
                renderer,
                audio,
                navigator,
                input,
                action_queue,
                gc_context,
                root,
                system_prototypes: avm.prototypes().clone(),
                mouse_hovered_object,
                mouse_position,
                drag_object,
                stage_size: (stage_width, stage_height),
            };

            let ret = f(avm, &mut update_context);

            // Hovered object may have been updated; copy it back to the GC root.
            root_data.mouse_hovered_object = update_context.mouse_hovered_object;
            ret
        })
    }

    /// Loads font data from the given buffer.
    /// The buffer should be the `DefineFont3` info for the tag.
    /// The tag header should not be included.
    fn load_device_font<'gc>(
        gc_context: gc_arena::MutationContext<'gc, '_>,
        data: &[u8],
        renderer: &mut Renderer,
    ) -> Result<crate::font::Font<'gc>, Error> {
        let mut reader = swf::read::Reader::new(data, 8);
        let device_font =
            crate::font::Font::from_swf_tag(gc_context, renderer, &reader.read_define_font_2(3)?)?;
        Ok(device_font)
    }
}

pub struct DragObject<'gc> {
    /// The display object being dragged.
    pub display_object: DisplayObject<'gc>,

    /// The offset from the mouse position to the center of the clip.
    pub offset: (Twips, Twips),

    /// The bounding rectangle where the clip will be maintained.
    pub constraint: BoundingBox,
}

unsafe impl<'gc> gc_arena::Collect for DragObject<'gc> {
    fn trace(&self, cc: gc_arena::CollectionContext) {
        self.display_object.trace(cc);
    }
}
