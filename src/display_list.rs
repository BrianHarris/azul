#![allow(unused_variables)]
#![allow(unused_macros)]

use webrender::api::*;
use app_units::{AU_PER_PX, MIN_AU, MAX_AU, Au};
use euclid::{TypedRect, TypedSize2D};
use cassowary::Constraint;

use {
    FastHashMap,
    resources::AppResources,
    traits::Layout,
    constraints::{DisplayRect, CssConstraint},
    ui_description::{UiDescription, StyledNode},
    window::UiSolver,
    window_state::WindowSize,
    id_tree::{Arena, NodeId},
    css_parser::*,
    dom::{NodeData, NodeType::{self, *}},
    css::Css,
    text_layout::{TextOverflowPass2, ScrollbarInfo},
    images::ImageId,
    text_cache::TextId,
    compositor::new_opengl_texture_id,
};

const DEFAULT_FONT_COLOR: TextColor = TextColor(ColorU { r: 0, b: 0, g: 0, a: 255 });
const DEFAULT_BUILTIN_FONT_SANS_SERIF: FontId = FontId::BuiltinFont("sans-serif");

pub(crate) struct DisplayList<'a, T: Layout + 'a> {
    pub(crate) ui_descr: &'a UiDescription<T>,
    pub(crate) rectangles: Arena<DisplayRectangle<'a>>
}

/// DisplayRectangle is the main type which the layout parsing step gets operated on.
#[derive(Debug)]
pub(crate) struct DisplayRectangle<'a> {
    /// `Some(id)` if this rectangle has a callback attached to it
    /// Note: this is not the same as the `NodeId`!
    /// These two are completely seperate numbers!
    pub tag: Option<u64>,
    /// The original styled node
    pub(crate) styled_node: &'a StyledNode,
    /// The style properties of the node, parsed
    pub(crate) style: RectStyle,
    /// The layout properties of the node, parsed
    pub(crate) layout: RectLayout,
}

/// It is not very efficient to re-create constraints on every call, the difference
/// in performance can be huge. Without re-creating constraints, solving can take 0.3 ms,
/// with re-creation it can take up to 9 ms. So the goal is to not re-create constraints
/// if their contents haven't changed.
#[derive(Default)]
pub(crate) struct SolvedLayout<T: Layout> {
    // List of previously solved constraints
    pub(crate) solved_constraints: FastHashMap<NodeId, NodeData<T>>,
}

/// This is used for caching large strings (in the `push_text` function)
/// In the cached version, you can lookup the text as well as the dimensions of
/// the words in the `AppResources`. For the `Uncached` version, you'll have to re-
/// calculate it on every frame.
///
/// TODO: It should be possible to switch this over to a `&'a str`, but currently
/// this leads to unsolvable borrowing issues.
pub(crate) enum TextInfo {
    Cached(TextId),
    Uncached(String),
}

impl TextInfo {
    /// Returns if the inner text is empty.
    ///
    /// Returns true if the TextInfo::Cached TextId does not exist
    /// (since in that case, it is "empty", so to speak)
    fn is_empty_text(&self, app_resources: &AppResources)
    -> bool
    {
        use self::TextInfo::*;

        match self {
            Cached(text_id) => {
                match app_resources.text_cache.string_cache.get(text_id) {
                    Some(s) => s.is_empty(),
                    None => true,
                }
            }
            Uncached(s) => s.is_empty(),
        }
    }
}

impl<T: Layout> SolvedLayout<T> {
    pub fn empty() -> Self {
        Self {
            solved_constraints: FastHashMap::default(),
        }
    }
}

impl<'a> DisplayRectangle<'a> {
    #[inline]
    pub fn new(tag: Option<u64>, styled_node: &'a StyledNode) -> Self {
        Self {
            tag: tag,
            styled_node: styled_node,
            style: RectStyle::default(),
            layout: RectLayout::default(),
        }
    }
}

impl<'a, T: Layout + 'a> DisplayList<'a, T> {

    /// NOTE: This function assumes that the UiDescription has an initialized arena
    ///
    /// This only looks at the user-facing styles of the `UiDescription`, not the actual
    /// layout. The layout is done only in the `into_display_list_builder` step.
    pub fn new_from_ui_description(ui_description: &'a UiDescription<T>) -> Self {

        let arena = ui_description.ui_descr_arena.borrow();
        let display_rect_arena = arena.transform(|node, node_id| {
            let style = ui_description.styled_nodes.get(&node_id).unwrap_or(&ui_description.default_style_of_node);
            let mut rect = DisplayRectangle::new(node.tag, style);
            populate_css_properties(&mut rect, &ui_description.dynamic_css_overrides);
            rect
        });

        Self {
            ui_descr: ui_description,
            rectangles: display_rect_arena,
        }
    }

    /// Looks if any new images need to be uploaded and stores the in the image resources
    fn update_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        Self::update_image_resources(api, app_resources, resource_updates);
        Self::update_font_resources(api, app_resources, resource_updates);
    }

    fn update_image_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        use images::{ImageState, ImageInfo};

        let mut updated_images = Vec::<(ImageId, (ImageData, ImageDescriptor))>::new();
        let mut to_delete_images = Vec::<(ImageId, Option<ImageKey>)>::new();

        // possible performance bottleneck (duplicated cloning) !!
        for (key, value) in app_resources.images.iter() {
            match *value {
                ImageState::ReadyForUpload(ref d) => {
                    updated_images.push((key.clone(), d.clone()));
                },
                ImageState::Uploaded(_) => { },
                ImageState::AboutToBeDeleted(ref k) => {
                    to_delete_images.push((key.clone(), k.clone()));
                }
            }
        }

        // Remove any images that should be deleted
        for (resource_key, image_key) in to_delete_images.into_iter() {
            if let Some(image_key) = image_key {
                resource_updates.push(ResourceUpdate::DeleteImage(image_key));
            }
            app_resources.images.remove(&resource_key);
        }

        // Upload all remaining images to the GPU only if the haven't been
        // uploaded yet
        for (resource_key, (data, descriptor)) in updated_images.into_iter() {

            let key = api.generate_image_key();
            resource_updates.push(ResourceUpdate::AddImage(
                AddImage { key, descriptor, data, tiling: None }
            ));

            *app_resources.images.get_mut(&resource_key).unwrap() =
                ImageState::Uploaded(ImageInfo {
                    key: key,
                    descriptor: descriptor
            });
        }
    }

    // almost the same as update_image_resources, but fonts
    // have two HashMaps that need to be updated
    fn update_font_resources(
        api: &RenderApi,
        app_resources: &mut AppResources,
        resource_updates: &mut Vec<ResourceUpdate>)
    {
        use font::FontState;
        use css_parser::FontId;

        let mut updated_fonts = Vec::<(FontId, Vec<u8>)>::new();
        let mut to_delete_fonts = Vec::<(FontId, Option<(FontKey, Vec<FontInstanceKey>)>)>::new();

        for (key, value) in app_resources.font_data.iter() {
            match value.2 {
                FontState::ReadyForUpload(ref bytes) => {
                    updated_fonts.push((key.clone(), bytes.clone()));
                },
                FontState::Uploaded(_) => { },
                FontState::AboutToBeDeleted(ref font_key) => {
                    let to_delete_font_instances = font_key.and_then(|f_key| {
                        let to_delete_font_instances = app_resources.fonts[&f_key].values().cloned().collect();
                        Some((f_key.clone(), to_delete_font_instances))
                    });
                    to_delete_fonts.push((key.clone(), to_delete_font_instances));
                }
            }
        }

        // Delete the complete font. Maybe a more granular option to
        // keep the font data in memory should be added later
        for (resource_key, to_delete_instances) in to_delete_fonts.into_iter() {
            if let Some((font_key, font_instance_keys)) = to_delete_instances {
                for instance in font_instance_keys {
                    resource_updates.push(ResourceUpdate::DeleteFontInstance(instance));
                }
                resource_updates.push(ResourceUpdate::DeleteFont(font_key));
                app_resources.fonts.remove(&font_key);
            }
            app_resources.font_data.remove(&resource_key);
        }

        // Upload all remaining fonts to the GPU only if the haven't been uploaded yet
        for (resource_key, data) in updated_fonts.into_iter() {
            let key = api.generate_font_key();
            resource_updates.push(ResourceUpdate::AddFont(AddFont::Raw(key, data, 0))); // TODO: use the index better?
            app_resources.font_data.get_mut(&resource_key).unwrap().2 = FontState::Uploaded(key);
        }
    }

    pub fn into_display_list_builder(
        &self,
        pipeline_id: PipelineId,
        current_epoch: Epoch,
        ui_solver: &mut UiSolver<T>,
        css: &mut Css,
        app_resources: &mut AppResources,
        render_api: &RenderApi,
        mut has_window_size_changed: bool,
        window_size: &WindowSize)
    -> Option<DisplayListBuilder>
    {
        let mut changeset = None;

        if let Some(root) = self.ui_descr.ui_descr_root {
            let local_changeset = ui_solver.dom_tree_cache.update(root, &*(self.ui_descr.ui_descr_arena.borrow()));
            ui_solver.edit_variable_cache.initialize_new_rectangles(&mut ui_solver.solver, &local_changeset);
            ui_solver.edit_variable_cache.remove_unused_variables(&mut ui_solver.solver);
            changeset = Some(local_changeset);
        }

        if css.needs_relayout {

            // constraints were added or removed during the last frame
            for rect_idx in self.rectangles.linear_iter() {
                let rect = &self.rectangles[rect_idx].data;
                let arena = &*self.ui_descr.ui_descr_arena.borrow();
                let dom_hash = &ui_solver.dom_tree_cache.previous_layout.arena[rect_idx];
                let display_rect = ui_solver.edit_variable_cache.map[&dom_hash.data];
                let layout_contraints = create_layout_constraints(rect, rect_idx, &self.rectangles, window_size);
                let cassowary_constraints = css_constraints_to_cassowary_constraints(&display_rect.1, &layout_contraints);
                ui_solver.solver.add_constraints(&cassowary_constraints).unwrap();
            }

            // if we push or pop constraints that means we also need to re-layout the window
            has_window_size_changed = true;
        }

        let changeset_is_useless = match changeset {
            None => true,
            Some(c) => c.is_empty()
        };

        // recalculate the actual layout
        if css.needs_relayout || has_window_size_changed {
            /*
            for change in solver.fetch_changes() {
                println!("change: - {:?}", change);
            }
            */
        }

        css.needs_relayout = false;

        use glium::glutin::dpi::LogicalSize;

        let LogicalSize { width, height } = window_size.dimensions;
        let mut builder = DisplayListBuilder::with_capacity(pipeline_id, TypedSize2D::new(width as f32, height as f32), self.rectangles.nodes_len());
        let mut resource_updates = Vec::<ResourceUpdate>::new();
        let full_screen_rect = LayoutRect::new(LayoutPoint::zero(), builder.content_size());;

        // Upload image and font resources
        Self::update_resources(render_api, app_resources, &mut resource_updates);

        for rect_idx in self.rectangles.linear_iter() {

            let arena = self.ui_descr.ui_descr_arena.borrow();
            let node_type = &arena[rect_idx].data.node_type;

            // ask the solver what the bounds of the current rectangle is
            // let bounds = ui_solver.query_bounds_of_rect(*rect_idx);

            // temporary: fill the whole window with each rectangle
            displaylist_handle_rect(
                &mut builder,
                current_epoch,
                rect_idx,
                &self.rectangles,
                node_type,
                full_screen_rect, /* replace this with the real bounds */
                full_screen_rect,
                app_resources,
                render_api,
                &mut resource_updates);
        }

        render_api.update_resources(resource_updates);

        Some(builder)
    }
}

fn displaylist_handle_rect<'a>(
    builder: &mut DisplayListBuilder,
    current_epoch: Epoch,
    rect_idx: NodeId,
    arena: &Arena<DisplayRectangle<'a>>,
    html_node: &NodeType,
    bounds: TypedRect<f32, LayoutPixel>,
    full_screen_rect: TypedRect<f32, LayoutPixel>,
    app_resources: &mut AppResources,
    render_api: &RenderApi,
    resource_updates: &mut Vec<ResourceUpdate>)
{
    let rect = &arena[rect_idx].data;

    let info = LayoutPrimitiveInfo {
        rect: bounds,
        clip_rect: bounds,
        is_backface_visible: false,
        tag: rect.tag.and_then(|tag| Some((tag, 0))),
    };

    let clip_region_id = rect.style.border_radius.and_then(|border_radius| {
        let region = ComplexClipRegion {
            rect: bounds,
            radii: border_radius,
            mode: ClipMode::Clip,
        };
        Some(builder.define_clip(bounds, vec![region], None))
    });

    // Push the "outset" box shadow, before the clip is active
    push_box_shadow(
        builder,
        &rect.style,
        &bounds,
        &full_screen_rect,
        BoxShadowClipMode::Outset);

    if let Some(id) = clip_region_id {
        builder.push_clip_id(id);
    }

    if let Some(ref bg_col) = rect.style.background_color {
        push_rect(&info, builder, bg_col);
    }

    if let Some(ref bg) = rect.style.background {
        push_background(
            &info,
            &bounds,
            builder,
            bg,
            &app_resources);
    };

    // Push the inset shadow (if any)
    push_box_shadow(builder,
                    &rect.style,
                    &bounds,
                    &full_screen_rect,
                    BoxShadowClipMode::Inset);

    push_border(
        &info,
        builder,
        &rect.style);

    let (horz_alignment, vert_alignment) = determine_text_alignment(rect_idx, arena);

    // handle the special content of the node
    match html_node {
        Div => { /* nothing special to do */ },
        Label(text) => {
            push_text(
                &info,
                &TextInfo::Uncached(text.clone()),
                builder,
                &rect.style,
                app_resources,
                &render_api,
                &bounds,
                resource_updates,
                horz_alignment,
                vert_alignment);
        },
        Text(text_id) => {
            push_text(
                &info,
                &TextInfo::Cached(*text_id),
                builder,
                &rect.style,
                app_resources,
                &render_api,
                &bounds,
                resource_updates,
                horz_alignment,
                vert_alignment);
        },
        Image(image_id) => {
            push_image(&info, builder, &bounds, app_resources, image_id);
        },
        GlTexture(texture) => {

            use compositor::{ActiveTexture, ACTIVE_GL_TEXTURES};

            let opaque = true;
            let allow_mipmaps = true;
            let descriptor = ImageDescriptor::new(texture.inner.width(), texture.inner.height(), ImageFormat::BGRA8, opaque, allow_mipmaps);
            let key = render_api.generate_image_key();
            let external_image_id = ExternalImageId(new_opengl_texture_id() as u64);

            let data = ImageData::External(ExternalImageData {
                id: external_image_id,
                channel_index: 0,
                image_type: ExternalImageType::TextureHandle(TextureTarget::Default),
            });

            ACTIVE_GL_TEXTURES.lock().unwrap()
                .entry(current_epoch).or_insert_with(|| FastHashMap::default())
                .insert(external_image_id, ActiveTexture { texture: texture.clone() });

            resource_updates.push(ResourceUpdate::AddImage(
                AddImage { key, descriptor, data, tiling: None }
            ));

            builder.push_image(
                &info,
                bounds.size,
                LayoutSize::zero(),
                ImageRendering::Auto,
                AlphaType::Alpha,
                key);
        },
    }

    if clip_region_id.is_some() {
        builder.pop_clip_id();
    }
}

/// For a given rectangle, determines what text alignment should be used
fn determine_text_alignment<'a>(rect_idx: NodeId, arena: &Arena<DisplayRectangle<'a>>)
-> (TextAlignmentHorz, TextAlignmentVert)
{
    let mut horz_alignment = TextAlignmentHorz::default();
    let mut vert_alignment = TextAlignmentVert::default();

    let rect = &arena[rect_idx];

    if let Some((Some(flex_direction), Some(justify_content))) = rect.parent.and_then(|parent| {
        let parent = &arena[parent];
        Some((parent.data.layout.direction, parent.data.layout.justify_content))
    }) {
        use css_parser::{LayoutDirection::*, LayoutJustifyContent::*};

        match flex_direction {
            Horizontal => {
                horz_alignment = match justify_content {
                    Start => TextAlignmentHorz::Left,
                    End => TextAlignmentHorz::Right,
                    Center | SpaceBetween | SpaceAround => TextAlignmentHorz::Center,
                };
            },
            Vertical => {
                vert_alignment = match justify_content {
                    Start => TextAlignmentVert::Top,
                    End => TextAlignmentVert::Bottom,
                    Center | SpaceBetween | SpaceAround => TextAlignmentVert::Center,
                };
            },
        }
    }

    if let Some(text_align) = rect.data.style.text_align {
        horz_alignment = text_align;
    }

    (horz_alignment, vert_alignment)
}

#[inline]
fn push_rect(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    color: &BackgroundColor)
{
    builder.push_rect(&info, color.0.into());
}

#[inline]
fn push_text(
    info: &PrimitiveInfo<LayoutPixel>,
    text: &TextInfo,
    builder: &mut DisplayListBuilder,
    style: &RectStyle,
    app_resources: &mut AppResources,
    render_api: &RenderApi,
    bounds: &TypedRect<f32, LayoutPixel>,
    resource_updates: &mut Vec<ResourceUpdate>,
    horz_alignment: TextAlignmentHorz,
    vert_alignment: TextAlignmentVert)
{
    use text_layout;

    if text.is_empty_text(&*app_resources) {
        return;
    }

    let font_family = match style.font_family {
        Some(ref ff) => ff,
        None => return,
    };

    let font_size = style.font_size.unwrap_or(DEFAULT_FONT_SIZE);
    let font_size_app_units = Au((font_size.0.to_pixels() as i32) * AU_PER_PX as i32);
    let font_id = font_family.fonts.get(0).unwrap_or(&DEFAULT_BUILTIN_FONT_SANS_SERIF);
    let font_result = push_font(font_id, font_size_app_units, resource_updates, app_resources, render_api);

    let font_instance_key = match font_result {
        Some(f) => f,
        None => return,
    };

    let line_height = style.line_height;

    let overflow_behaviour = style.overflow.unwrap_or(LayoutOverflow::default());

    let scrollbar_style = ScrollbarInfo {
        width: 17,
        padding: 2,
        background_color: BackgroundColor(ColorU { r: 241, g: 241, b: 241, a: 255 }),
        triangle_color: BackgroundColor(ColorU { r: 163, g: 163, b: 163, a: 255 }),
        bar_color: BackgroundColor(ColorU { r: 193, g: 193, b: 193, a: 255 }),
    };

    let (positioned_glyphs, scrollbar_info) = text_layout::get_glyphs(
        app_resources,
        bounds,
        horz_alignment,
        vert_alignment,
        &font_id,
        &font_size,
        line_height,
        text,
        &overflow_behaviour,
        &scrollbar_style
    );

    let font_color = style.font_color.unwrap_or(DEFAULT_FONT_COLOR).0.into();
    let mut flags = FontInstanceFlags::empty();
    flags.set(FontInstanceFlags::SUBPIXEL_BGR, true);
    flags.set(FontInstanceFlags::FONT_SMOOTHING, true);
    flags.set(FontInstanceFlags::FORCE_AUTOHINT, true);
    flags.set(FontInstanceFlags::LCD_VERTICAL, true);

    let options = GlyphOptions {
        render_mode: FontRenderMode::Subpixel,
        flags: flags,
    };

    builder.push_text(&info, &positioned_glyphs, font_instance_key, font_color, Some(options));

    use text_layout::TextOverflow;

    // If the rectangle should have a scrollbar, push a scrollbar onto the display list
    // TODO !!!
    if let TextOverflow::IsOverflowing(amount_vert) = scrollbar_info.vertical {
        push_scrollbar(builder, &overflow_behaviour, &scrollbar_info, &scrollbar_style, bounds, &style.border)
    }
    if let TextOverflow::IsOverflowing(amount_horz) = scrollbar_info.horizontal {
        push_scrollbar(builder, &overflow_behaviour, &scrollbar_info, &scrollbar_style, bounds, &style.border)
    }
}

/// Adds a scrollbar to the left or bottom side of a rectangle.
/// TODO: make styling configurable (like the width / style of the scrollbar)
fn push_scrollbar(
    builder: &mut DisplayListBuilder,
    display_behaviour: &LayoutOverflow,
    scrollbar_info: &TextOverflowPass2,
    scrollbar_style: &ScrollbarInfo,
    bounds: &TypedRect<f32, LayoutPixel>,
    border: &Option<(BorderWidths, BorderDetails)>)
{
    use euclid::TypedPoint2D;

    // The border is inside the rectangle - subtract the border width on the left and bottom side,
    // so that the scrollbar is laid out correctly
    let mut bounds = *bounds;
    if let Some((border_widths, _)) = border {
        bounds.size.width -= border_widths.left;
        bounds.size.height -= border_widths.bottom;
    }

    {
        // Background of scrollbar (vertical)
        let scrollbar_vertical_background = TypedRect::<f32, LayoutPixel> {
            origin: TypedPoint2D::new(bounds.origin.x + bounds.size.width - scrollbar_style.width as f32, bounds.origin.y),
            size: TypedSize2D::new(scrollbar_style.width as f32, bounds.size.height),
        };

        let scrollbar_vertical_background_info = PrimitiveInfo {
            rect: scrollbar_vertical_background,
            clip_rect: bounds,
            is_backface_visible: false,
            tag: None, // TODO: for hit testing
        };

        push_rect(&scrollbar_vertical_background_info, builder, &scrollbar_style.background_color);
    }

    {
        // Actual scroll bar
        let scrollbar_vertical_bar = TypedRect::<f32, LayoutPixel> {
            origin: TypedPoint2D::new(
                bounds.origin.x + bounds.size.width - scrollbar_style.width as f32 + scrollbar_style.padding as f32,
                bounds.origin.y + scrollbar_style.width as f32),
            size: TypedSize2D::new(
                (scrollbar_style.width - (scrollbar_style.padding * 2)) as f32,
                 bounds.size.height - (scrollbar_style.width * 2) as f32),
        };

        let scrollbar_vertical_bar_info = PrimitiveInfo {
            rect: scrollbar_vertical_bar,
            clip_rect: bounds,
            is_backface_visible: false,
            tag: None, // TODO: for hit testing
        };

        push_rect(&scrollbar_vertical_bar_info, builder, &scrollbar_style.bar_color);
    }

    {
        // Triangle top
        let mut scrollbar_triangle_rect = TypedRect::<f32, LayoutPixel> {
            origin: TypedPoint2D::new(
                bounds.origin.x + bounds.size.width - scrollbar_style.width as f32 + scrollbar_style.padding as f32,
                bounds.origin.y + scrollbar_style.padding as f32),
            size: TypedSize2D::new(
                (scrollbar_style.width - (scrollbar_style.padding * 2)) as f32,
                (scrollbar_style.width - (scrollbar_style.padding * 2)) as f32),
        };

        scrollbar_triangle_rect.origin.x += scrollbar_triangle_rect.size.width / 4.0;
        scrollbar_triangle_rect.origin.y += scrollbar_triangle_rect.size.height / 4.0;
        scrollbar_triangle_rect.size.width /= 2.0;
        scrollbar_triangle_rect.size.height /= 2.0;

        push_triangle(&scrollbar_triangle_rect, builder, &scrollbar_style.triangle_color, TriangleDirection::PointUp);

        // Triangle bottom
        scrollbar_triangle_rect.origin.y += bounds.size.height - scrollbar_style.width as f32 + scrollbar_style.padding as f32;
        push_triangle(&scrollbar_triangle_rect, builder, &scrollbar_style.triangle_color, TriangleDirection::PointDown);
    }
}

enum TriangleDirection {
    PointUp,
    PointDown,
    PointRight,
    PointLeft,
}

fn push_triangle(
    bounds: &TypedRect<f32, LayoutPixel>,
    builder: &mut DisplayListBuilder,
    background_color: &BackgroundColor,
    direction: TriangleDirection)
{
    use self::TriangleDirection::*;

    // see: https://css-tricks.com/snippets/css/css-triangle/
    // uses the "3d effect" for making a triangle

    let triangle_rect_info = PrimitiveInfo {
        rect: *bounds,
        clip_rect: *bounds,
        is_backface_visible: false,
        tag: None,
    };

    const TRANSPARENT: ColorU = ColorU { r: 0,    b: 0,   g: 0,   a: 0  };

    // make all borders but one transparent
    let [b_left, b_right, b_top, b_bottom] = match direction {
        PointUp         => [(TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden), (background_color.0, BorderStyle::Solid) ],
        PointDown       => [(TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden), (background_color.0, BorderStyle::Solid),  (TRANSPARENT, BorderStyle::Hidden)],
        PointLeft       => [(TRANSPARENT, BorderStyle::Hidden), (background_color.0, BorderStyle::Solid),  (TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden)],
        PointRight      => [(background_color.0, BorderStyle::Solid),  (TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden), (TRANSPARENT, BorderStyle::Hidden)],
    };

    let border_details = BorderDetails::Normal(NormalBorder {
        left:   BorderSide { color: b_left.0.into(),         style: b_left.1   },
        right:  BorderSide { color: b_right.0.into(),        style: b_right.1  },
        top:    BorderSide { color: b_top.0.into(),          style: b_top.1    },
        bottom: BorderSide { color: b_bottom.0.into(),       style: b_bottom.1 },
        radius: BorderRadius::zero(),
    });

    // make the borders half the width / height of the rectangle,
    // so that the border looks like a triangle
    let border_widths = BorderWidths {
        left: bounds.size.width / 2.0,
        top: bounds.size.height / 2.0,
        right: bounds.size.width / 2.0,
        bottom: bounds.size.height / 2.0,
    };

    builder.push_border(&triangle_rect_info, border_widths, border_details);
}

/// WARNING: For "inset" shadows, you must push a clip ID first, otherwise the
/// shadow will not show up.
///
/// To prevent a shadow from being pushed twice, you have to annotate the clip
/// mode for this - outset or inset.
#[inline]
fn push_box_shadow(
    builder: &mut DisplayListBuilder,
    style: &RectStyle,
    bounds: &TypedRect<f32, LayoutPixel>,
    full_screen_rect: &TypedRect<f32, LayoutPixel>,
    shadow_type: BoxShadowClipMode)
{
    let pre_shadow = match style.box_shadow {
        Some(ref ps) => ps,
        None => return,
    };

    // The pre_shadow is missing the BorderRadius & LayoutRect
    let border_radius = style.border_radius.unwrap_or(BorderRadius::zero());

    if pre_shadow.clip_mode != shadow_type {
        return;
    }

    let clip_rect = if pre_shadow.clip_mode == BoxShadowClipMode::Inset {
        // inset shadows do not work like outset shadows
        // for inset shadows, you have to push a clip ID first, so that they are
        // clipped to the bounds -we trust that the calling function knows to do this
        *bounds
    } else {
        // calculate the maximum extent of the outset shadow
        let mut clip_rect = *bounds;

        let origin_displace = pre_shadow.spread_radius - pre_shadow.blur_radius;
        clip_rect.origin.x = clip_rect.origin.x + pre_shadow.offset.x - origin_displace;
        clip_rect.origin.y = clip_rect.origin.y + pre_shadow.offset.y - origin_displace;

        let spread = (pre_shadow.spread_radius * 2.0) + (pre_shadow.blur_radius * 2.0);
        clip_rect.size.height = clip_rect.size.height + spread;
        clip_rect.size.width = clip_rect.size.width + spread;

        // prevent shadows that are larger than the full screen
        clip_rect.intersection(full_screen_rect).unwrap_or(clip_rect)
    };

    let info = LayoutPrimitiveInfo::with_clip_rect(LayoutRect::zero(), clip_rect);
    builder.push_box_shadow(&info, *bounds, pre_shadow.offset, pre_shadow.color,
                             pre_shadow.blur_radius, pre_shadow.spread_radius,
                             border_radius, pre_shadow.clip_mode);
}

#[inline]
fn push_background(
    info: &PrimitiveInfo<LayoutPixel>,
    bounds: &TypedRect<f32, LayoutPixel>,
    builder: &mut DisplayListBuilder,
    background: &Background,
    app_resources: &AppResources)
{
    match background {
        Background::RadialGradient(gradient) => {
            let mut stops: Vec<GradientStop> = gradient.stops.iter().map(|gradient_pre|
                GradientStop {
                    offset: gradient_pre.offset.unwrap(),
                    color: gradient_pre.color,
                }).collect();
            let center = bounds.bottom_left(); // TODO - expose in CSS
            let radius = TypedSize2D::new(40.0, 40.0); // TODO - expose in CSS
            let gradient = builder.create_radial_gradient(center, radius, stops, gradient.extend_mode);
            builder.push_radial_gradient(&info, gradient, bounds.size, LayoutSize::zero());
        },
        Background::LinearGradient(gradient) => {
            let mut stops: Vec<GradientStop> = gradient.stops.iter().map(|gradient_pre|
                GradientStop {
                    offset: gradient_pre.offset.unwrap(),
                    color: gradient_pre.color,
                }).collect();
            let (begin_pt, end_pt) = gradient.direction.to_points(&bounds);
            let gradient = builder.create_gradient(begin_pt, end_pt, stops, gradient.extend_mode);
            builder.push_gradient(&info, gradient, bounds.size, LayoutSize::zero());
        },
        Background::Image(css_image_id) => {
            if let Some(image_id) = app_resources.css_ids_to_image_ids.get(&css_image_id.0) {
                push_image(info, builder, bounds, app_resources, image_id);
            }
        },
        Background::NoBackground => { },
    }
}

fn push_image(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    bounds: &TypedRect<f32, LayoutPixel>,
    app_resources: &AppResources,
    image_id: &ImageId)
{
    if let Some(image_info) = app_resources.images.get(image_id) {
        use images::ImageState::*;
        match image_info {
            Uploaded(image_info) => {
                builder.push_image(
                        &info,
                        bounds.size,
                        LayoutSize::zero(),
                        ImageRendering::Auto,
                        AlphaType::Alpha,
                        image_info.key);
            },
            _ => { },
        }
    }
}

#[inline]
fn push_border(
    info: &PrimitiveInfo<LayoutPixel>,
    builder: &mut DisplayListBuilder,
    style: &RectStyle)
{
    if let Some((border_widths, mut border_details)) = style.border {
        if let Some(border_radius) = style.border_radius {
            if let BorderDetails::Normal(ref mut n) = border_details {
                n.radius = border_radius;
            }
        }
        builder.push_border(info, border_widths, border_details);
    }
}

#[inline]
fn push_font(
    font_id: &FontId,
    font_size_app_units: Au,
    resource_updates: &mut Vec<ResourceUpdate>,
    app_resources: &mut AppResources,
    render_api: &RenderApi)
-> Option<FontInstanceKey>
{
    use font::FontState;

    if font_size_app_units < MIN_AU || font_size_app_units > MAX_AU {
        error!("warning: too big or too small font size");
        return None;
    }

    let &(ref font, _, ref font_state) = match app_resources.font_data.get(font_id) {
        Some(f) => f,
        None => return None,
    };

    match *font_state {
        FontState::Uploaded(font_key) => {
            let font_sizes_hashmap = app_resources.fonts.entry(font_key)
                                     .or_insert(FastHashMap::default());
            let font_instance_key = font_sizes_hashmap.entry(font_size_app_units)
                .or_insert_with(|| {
                    let f_instance_key = render_api.generate_font_instance_key();
                    resource_updates.push(ResourceUpdate::AddFontInstance(
                        AddFontInstance {
                            key: f_instance_key,
                            font_key: font_key,
                            glyph_size: font_size_app_units,
                            options: None,
                            platform_options: None,
                            variations: Vec::new(),
                        }
                    ));
                    f_instance_key
                }
            );

            Some(*font_instance_key)
        },
        _ => {
            error!("warning: trying to use font {:?} that isn't available", font_id);
            None
        },
    }
}

/// Populate and parse the CSS style properties
fn populate_css_properties(rect: &mut DisplayRectangle, css_overrides: &FastHashMap<String, ParsedCssProperty>)
{
    use css_parser::ParsedCssProperty::{self, *};

    fn apply_parsed_css_property(rect: &mut DisplayRectangle, property: &ParsedCssProperty) {
        match property {
            BorderRadius(b)             => { rect.style.border_radius = Some(*b);                   },
            BackgroundColor(c)          => { rect.style.background_color = Some(*c);                },
            TextColor(t)                => { rect.style.font_color = Some(*t);                      },
            Border(widths, details)     => { rect.style.border = Some((*widths, *details));         },
            Background(b)               => { rect.style.background = Some(b.clone());               },
            FontSize(f)                 => { rect.style.font_size = Some(*f);                       },
            FontFamily(f)               => { rect.style.font_family = Some(f.clone());              },
            Overflow(o)                 => {
                if let Some(ref mut existing_overflow) = rect.style.overflow {
                    existing_overflow.merge(o);
                } else {
                    rect.style.overflow = Some(*o)
                }
            },
            TextAlign(ta)               => { rect.style.text_align = Some(*ta);                     },
            BoxShadow(opt_box_shadow)   => { rect.style.box_shadow = *opt_box_shadow;               },
            LineHeight(lh)              => { rect.style.line_height = Some(*lh);                     },

            Width(w)                    => { rect.layout.width = Some(*w);                          },
            Height(h)                   => { rect.layout.height = Some(*h);                         },
            MinWidth(mw)                => { rect.layout.min_width = Some(*mw);                     },
            MinHeight(mh)               => { rect.layout.min_height = Some(*mh);                    },
            MaxWidth(mw)                => { rect.layout.max_width = Some(*mw);                     },
            MaxHeight(mh)               => { rect.layout.max_height = Some(*mh);                    },

            FlexWrap(w)                 => { rect.layout.wrap = Some(*w);                           },
            FlexDirection(d)            => { rect.layout.direction = Some(*d);                      },
            JustifyContent(j)           => { rect.layout.justify_content = Some(*j);                },
            AlignItems(a)               => { rect.layout.align_items = Some(*a);                    },
            AlignContent(a)             => { rect.layout.align_content = Some(*a);                  },
        }
    }

    // Assert that the types of two properties matches
    fn property_type_matches(a: &ParsedCssProperty, b: &ParsedCssProperty) -> bool {
        use std::mem::discriminant;
        discriminant(a) == discriminant(b)
    }

    for constraint in &rect.styled_node.css_constraints.list {
        use css::CssDeclaration::*;
        match constraint {
            Static(static_property) => apply_parsed_css_property(rect, static_property),
            Dynamic(dynamic_property) => {
                let calculated_property = css_overrides.get(&dynamic_property.dynamic_id);
                if let Some(overridden_property) = calculated_property {
                    assert!(property_type_matches(overridden_property, &dynamic_property.default),
                            "css values don't have the same discriminant type");
                    apply_parsed_css_property(rect, overridden_property);
                } else {
                    apply_parsed_css_property(rect, &dynamic_property.default);
                }
            }
        }
    }
}

// Returns the constraints for one rectangle
fn create_layout_constraints<'a>(
    rect: &DisplayRectangle,
    rect_id: NodeId,
    arena: &Arena<DisplayRectangle<'a>>,
    window_size: &WindowSize)
-> Vec<CssConstraint>
{
    use cassowary::strength::*;
    use constraints::{SizeConstraint, Strength};

    let mut layout_constraints = Vec::<CssConstraint>::new();
    /*
    let max_width = arena.get_wh_for_rectangle(rect_id, WidthOrHeight::Width)
                         .unwrap_or(window_size.width as f32);
    */
    layout_constraints.push(CssConstraint::Size((SizeConstraint::Width(200.0), Strength(STRONG))));
    layout_constraints.push(CssConstraint::Size((SizeConstraint::Height(200.0), Strength(STRONG))));

    layout_constraints
}

fn css_constraints_to_cassowary_constraints(rect: &DisplayRect, css: &Vec<CssConstraint>)
-> Vec<Constraint>
{
    use self::CssConstraint::*;

    css.iter().flat_map(|constraint|
        match *constraint {
            Size((constraint, strength)) => {
                constraint.build(&rect, strength.0)
            }
            Padding((constraint, strength, padding)) => {
                constraint.build(&rect, strength.0, padding.0)
            }
        }
    ).collect()
}

// Layout / tracing-related functions

// What constraint (width or height) to search for when looking for a fitting width / height constraint
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum WidthOrHeight {
    Width,
    Height,
}

impl<'a> Arena<DisplayRectangle<'a>> {

    /// Recursive algorithm for getting the dimensions of a rectangle
    ///
    /// This function can be used on any rectangle to get the maximum allowed width
    /// (for inserting the width / height constraint into the layout solver).
    /// It simply traverses upwards through the nodes, until it finds a matching min-width / width
    /// constraint, returns None, if the root node is reached (with no constraints)
    ///
    /// Usually, you'd use it like:
    ///
    /// ```no_run,ignore
    /// let max_width = arena.get_wh_for_rectangle(id, WidthOrHeight::Width)
    ///                      .unwrap_or(window_dimensions.width);
    /// ```
    fn get_wh_for_rectangle(&self, id: NodeId, field: WidthOrHeight) -> Option<f32> {

        use self::WidthOrHeight::*;

        let node = &self[id];

        macro_rules! get_wh {
            ($field_name:ident, $min_field:ident) => ({
                let mut $field_name: Option<f32> = None;

                match node.data.layout.$min_field {
                    Some(m_w) => {
                        let m_w_px = m_w.0.to_pixels();
                        match node.data.layout.$field_name {
                            Some(w) => {
                                // width + min_width
                                let w_px = w.0.to_pixels();
                                $field_name = Some(m_w_px.max(w_px));
                            },
                            None => {
                                // min_width
                                $field_name = Some(m_w_px);
                            }
                        }
                    },
                    None => {
                        match node.data.layout.$field_name {
                            Some(w) => {
                                // width
                                let w_px = w.0.to_pixels();
                                $field_name = Some(w_px);
                            },
                            None => {
                                // neither width nor min_width
                            }
                        }
                    }
                };

                if $field_name.is_none() {
                    match node.parent() {
                        Some(p) => $field_name = self.get_wh_for_rectangle(p, field),
                        None => { },
                    }
                }

                $field_name
            })
        }

        match field {
            Width => {
                let w = get_wh!(width, min_width);
                w
            },
            Height => {
                let h = get_wh!(height, min_height);
                h
            }
        }
    }
}

// Empty test, for some reason codecov doesn't detect any files (and therefore
// doesn't report codecov % correctly) except if they have at least one test in
// the file. This is an empty test, which should be updated later on
#[test]
fn __codecov_test_display_list_file() {

}
