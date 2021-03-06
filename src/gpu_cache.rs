//! This module provides capabilities for managing a cache of rendered glyphs in
//! GPU memory, with the goal of minimisng the size and frequency of glyph
//! uploads to GPU memory from the CPU.
//!
//! This module is optional, and not compiled by default. To use it enable the
//! `gpu_cache` feature in your Cargo.toml.
//!
//! Typical applications that render directly with hardware graphics APIs (e.g.
//! games) need text rendering. There is not yet a performant solution for high
//! quality text rendering directly on the GPU that isn't experimental research
//! work. Quality is often critical for legibility, so many applications use
//! text or individual characters that have been rendered on the CPU. This is
//! done either ahead-of-time, giving a fixed set of fonts, characters, and
//! sizes that can be used at runtime, or dynamically as text is required. This
//! latter scenario is more flexible and the focus of this module.
//!
//! To minimise the CPU load and texture upload bandwidth saturation, recently
//! used glyphs should be cached on the GPU for use by future frames. This
//! module provides a mechanism for maintaining such a cache in the form of a
//! single packed 2D GPU texture. When a rendered glyph is requested, it is
//! either retrieved from its location in the texture if it is present or room
//! is made in the cache (if necessary), the CPU renders the glyph then it is
//! uploaded into a gap in the texture to be available for GPU rendering. This
//! cache uses a Least Recently Used (LRU) cache eviction scheme - glyphs in the
//! cache that have not been used recently are as a rule of thumb not likely to
//! be used again soon, so they are the best candidates for eviction to make
//! room for required glyphs.
//!
//! The API for the cache does not assume a particular graphics API. The
//! intended usage is to queue up glyphs that need to be present for the current
//! frame using `Cache::queue_glyph`, update the cache to ensure that the queued
//! glyphs are present using `Cache::cache_queued` (providing a function for
//! uploading pixel data), then when it's time to render call `Cache::rect_for`
//! to get the UV coordinates in the cache texture for each glyph. For a
//! concrete use case see the `gpu_cache` example.
//!
//! Cache dimensions are immutable. If you need to change the dimensions of the
//! cache texture (e.g. due to high cache pressure), construct a new `Cache`
//! and discard the old one.

extern crate fnv;
extern crate linked_hash_map;

use self::fnv::{FnvBuildHasher, FnvHashMap};
use self::linked_hash_map::LinkedHashMap;
use {GlyphId, PositionedGlyph, Rect, Scale, Vector};
use ordered_float::OrderedFloat;
use point;
use std::cmp::{Ord, Ordering, PartialOrd};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::collections::Bound::{Included, Unbounded};
use std::error;
use std::fmt;

/// Texture coordinates (floating point) of the quad for a glyph in the cache,
/// as well as the pixel-space (integer) coordinates that this region should be
/// drawn at.
pub type TextureCoords = (Rect<f32>, Rect<i32>);
type FontId = usize;

/// Indicates where a glyph texture is stored in the cache
/// (row position, glyph index in row)
type TextureRowGlyphIndex = (u32, u32);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct GlyphScaleOffset {
    scale: (OrderedFloat<f32>, OrderedFloat<f32>),
    offset: (OrderedFloat<f32>, OrderedFloat<f32>),
}

impl PartialOrd for GlyphScaleOffset {
    fn partial_cmp(&self, other: &GlyphScaleOffset) -> Option<Ordering> {
        (self.scale, self.offset).partial_cmp(&(other.scale, other.offset))
    }
}

impl Ord for GlyphScaleOffset {
    fn cmp(&self, other: &GlyphScaleOffset) -> Ordering {
        (self.scale, self.offset).cmp(&(other.scale, other.offset))
    }
}

impl GlyphScaleOffset {
    fn new(scale: Scale, offset: Vector<f32>) -> Self {
        Self {
            scale: (OrderedFloat(scale.x), OrderedFloat(scale.y)),
            offset: (OrderedFloat(offset.x), OrderedFloat(offset.y)),
        }
    }

    fn offset(&self) -> Vector<f32> {
        Vector {
            x: *self.offset.0,
            y: *self.offset.1,
        }
    }

    /// Returns if this cached glyph can be considered to match another at
    /// input tolerances
    #[inline]
    fn matches(
        &self,
        other: &GlyphScaleOffset,
        scale_tolerance: f32,
        position_tolerance: f32,
    ) -> bool {
        (*self.scale.0 - *other.scale.0).abs() < scale_tolerance
            && (*self.scale.1 - *other.scale.1).abs() < scale_tolerance
            && (*self.offset.0 - *other.offset.0).abs() < position_tolerance
            && (*self.offset.1 - *other.offset.1).abs() < position_tolerance
    }

    #[inline]
    fn match_distance(
        &self,
        other: &GlyphScaleOffset,
        scale_tolerance: f32,
        position_tolerance: f32,
    ) -> f32 {
        ((*self.scale.0 - *other.scale.0) / scale_tolerance).abs()
            + ((*self.scale.1 - *other.scale.1) / scale_tolerance).abs()
            + ((*self.offset.0 - *other.offset.0) / position_tolerance).abs()
            + ((*self.offset.1 - *other.offset.1) / position_tolerance).abs()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ByteArray2d {
    inner_array: Vec<u8>,
    row: usize,
    col: usize,
}

impl ByteArray2d {
    pub fn zeros(row: usize, col: usize) -> Self {
        ByteArray2d {
            inner_array: vec![0; row * col],
            row,
            col,
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.inner_array.as_slice()
    }

    fn get_vec_index(&self, row: usize, col: usize) -> usize {
        if row >= self.row {
            panic!("row out of range: row={}, given={}", self.row, row);
        } else if col >= self.col {
            panic!("column out of range: col={}, given={}", self.col, col);
        } else {
            row * self.col + col
        }
    }
}

impl ::std::ops::Index<(usize, usize)> for ByteArray2d {
    type Output = u8;

    fn index(&self, (row, col): (usize, usize)) -> &u8 {
        &self.inner_array[self.get_vec_index(row, col)]
    }
}

impl ::std::ops::IndexMut<(usize, usize)> for ByteArray2d {
    fn index_mut(&mut self, (row, col): (usize, usize)) -> &mut u8 {
        let vec_index = self.get_vec_index(row, col);
        &mut self.inner_array[vec_index]
    }
}

/// Row of pixel data
struct Row {
    /// Row pixel height
    height: u32,
    /// Pixel width current in use by glyphs
    width: u32,
    glyphs: Vec<GlyphTexInfo>,
}

struct GlyphTexInfo {
    font_glyph: (FontId, GlyphId),
    scale_offset: GlyphScaleOffset,
    tex_coords: Rect<u32>,
}

trait PaddingAware {
    fn unpadded(self) -> Self;
}

impl PaddingAware for Rect<u32> {
    /// A padded texture has 1 extra pixel on all sides
    fn unpadded(mut self) -> Self {
        self.min.x += 1;
        self.min.y += 1;
        self.max.x -= 1;
        self.max.y -= 1;
        self
    }
}

/// An implementation of a dynamic GPU glyph cache. See the module documentation
/// for more information.
pub struct Cache<'font> {
    scale_tolerance: f32,
    position_tolerance: f32,
    width: u32,
    height: u32,
    rows: LinkedHashMap<u32, Row, FnvBuildHasher>,
    /// Mapping of row gaps bottom -> top
    space_start_for_end: FnvHashMap<u32, u32>,
    /// Mapping of row gaps top -> bottom
    space_end_for_start: FnvHashMap<u32, u32>,
    queue: Vec<(FontId, PositionedGlyph<'font>)>,
    queue_retry: bool,
    all_glyphs: FnvHashMap<(FontId, GlyphId), BTreeMap<GlyphScaleOffset, TextureRowGlyphIndex>>,
    pad_glyphs: bool,
}

/// Builder for a `Cache`.
///
/// # Example
///
/// ```
/// use rusttype::gpu_cache::CacheBuilder;
///
/// let default_cache = CacheBuilder {
///     width: 256,
///     height: 256,
///     scale_tolerance: 0.1,
///     position_tolerance: 0.1,
///     pad_glyphs: true,
/// }.build();
///
/// let bigger_cache = CacheBuilder {
///     width: 1024,
///     height: 1024,
///     ..CacheBuilder::default()
/// }.build();
/// # let (_, _) = (default_cache, bigger_cache);
/// ```
#[derive(Debug, Clone)]
pub struct CacheBuilder {
    /// Along with `height` specifies the dimensions of the 2D texture that will
    /// hold the cache contents on the GPU.
    ///
    /// This must match the dimensions of the actual texture used, otherwise
    /// `cache_queued` will try to cache into coordinates outside the bounds of
    /// the texture.
    pub width: u32,
    /// Along with `width` specifies the dimensions of the 2D texture that will
    /// hold the cache contents on the GPU.
    ///
    /// This must match the dimensions of the actual texture used, otherwise
    /// `cache_queued` will try to cache into coordinates outside the bounds of
    /// the texture.
    pub height: u32,
    /// Specifies the tolerances (maximum allowed difference) for judging
    /// whether an existing glyph in the cache is close enough to the
    /// requested glyph in scale to be used in its place. Due to floating
    /// point inaccuracies that can affect user code it is not recommended
    /// to set these parameters too close to zero as effectively identical
    /// glyphs could end up duplicated in the cache.
    ///
    /// Both `scale_tolerance` and `position_tolerance` are measured in pixels.
    ///
    /// A typical application will produce results with no perceptible
    /// inaccuracies with `scale_tolerance` and `position_tolerance` set to
    /// 0.1. Depending on the target DPI higher tolerance may be acceptable.
    pub scale_tolerance: f32,
    /// Specifies the tolerances (maximum allowed difference) for judging
    /// whether an existing glyph in the cache is close enough to the requested
    /// glyph in subpixel offset to be used in its place. Due to floating point
    /// inaccuracies that can affect user code it is not recommended to set
    /// these parameters too close to zero as effectively identical glyphs
    /// could end up duplicated in the cache.
    ///
    /// Both `scale_tolerance` and `position_tolerance` are measured in pixels.
    ///
    /// Note that since `position_tolerance` is a tolerance of subpixel
    /// offsets, setting it to 1.0 or higher is effectively a "don't care"
    /// option.
    ///
    /// A typical application will produce results with no perceptible
    /// inaccuracies with `scale_tolerance` and `position_tolerance` set to
    /// 0.1. Depending on the target DPI higher tolerance may be acceptable.
    pub position_tolerance: f32,
    /// Pack glyphs in texture with a padding of a single zero alpha pixel to
    /// avoid bleeding from interpolated shader texture lookups near edges.
    ///
    /// If glyphs are never transformed this may be set to `false` to slightly
    /// improve the glyph packing.
    pub pad_glyphs: bool,
}

impl Default for CacheBuilder {
    fn default() -> Self {
        Self {
            width: 256,
            height: 256,
            scale_tolerance: 0.1,
            position_tolerance: 0.1,
            pad_glyphs: true,
        }
    }
}

impl CacheBuilder {
    /// Constructs a new cache. Note that this is just the CPU side of the
    /// cache. The GPU texture is managed by the user.
    ///
    /// # Panics
    ///
    /// `scale_tolerance` or `position_tolerance` are less than or equal to
    /// zero.
    pub fn build<'a>(self) -> Cache<'a> {
        let CacheBuilder {
            width,
            height,
            scale_tolerance,
            position_tolerance,
            pad_glyphs,
        } = self;
        assert!(scale_tolerance >= 0.0);
        assert!(position_tolerance >= 0.0);
        let scale_tolerance = scale_tolerance.max(0.001);
        let position_tolerance = position_tolerance.max(0.001);

        Cache {
            scale_tolerance,
            position_tolerance,
            width,
            height,
            rows: LinkedHashMap::default(),
            space_start_for_end: {
                let mut m = HashMap::default();
                m.insert(height, 0);
                m
            },
            space_end_for_start: {
                let mut m = HashMap::default();
                m.insert(0, height);
                m
            },
            queue: Vec::new(),
            queue_retry: false,
            all_glyphs: HashMap::default(),
            pad_glyphs,
        }
    }
}

/// Returned from `Cache::rect_for`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheReadErr {
    /// Indicates that the requested glyph is not present in the cache
    GlyphNotCached,
}
impl fmt::Display for CacheReadErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", error::Error::description(self))
    }
}
impl error::Error for CacheReadErr {
    fn description(&self) -> &str {
        match *self {
            CacheReadErr::GlyphNotCached => "Glyph not cached",
        }
    }
}

/// Returned from `Cache::cache_queued`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CacheWriteErr {
    /// At least one of the queued glyphs is too big to fit into the cache, even
    /// if all other glyphs are removed.
    GlyphTooLarge,
    /// Not all of the requested glyphs can fit into the cache, even if the
    /// cache is completely cleared before the attempt.
    NoRoomForWholeQueue,
}
impl fmt::Display for CacheWriteErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", error::Error::description(self))
    }
}
impl error::Error for CacheWriteErr {
    fn description(&self) -> &str {
        match *self {
            CacheWriteErr::GlyphTooLarge => "Glyph too large",
            CacheWriteErr::NoRoomForWholeQueue => "No room for whole queue",
        }
    }
}

fn normalise_pixel_offset(mut offset: Vector<f32>) -> Vector<f32> {
    if offset.x > 0.5 {
        offset.x -= 1.0;
    } else if offset.x < -0.5 {
        offset.x += 1.0;
    }
    if offset.y > 0.5 {
        offset.y -= 1.0;
    } else if offset.y < -0.5 {
        offset.y += 1.0;
    }
    offset
}

impl<'font> Cache<'font> {
    /// Legacy `Cache` construction, use `CacheBuilder` for more options.
    ///
    /// # Panics
    ///
    /// `scale_tolerance` or `position_tolerance` are less than or equal to
    /// zero.
    pub fn new<'a>(
        width: u32,
        height: u32,
        scale_tolerance: f32,
        position_tolerance: f32,
    ) -> Cache<'a> {
        CacheBuilder {
            width,
            height,
            scale_tolerance,
            position_tolerance,
            pad_glyphs: false,
        }.build()
    }

    /// Sets the scale tolerance for the cache. See the documentation for
    /// `CacheBuilder` for more information.
    ///
    /// # Panics
    ///
    /// `tolerance` is less than or equal to zero.
    pub fn set_scale_tolerance(&mut self, tolerance: f32) {
        assert!(tolerance >= 0.0);
        let tolerance = tolerance.max(0.001);
        self.scale_tolerance = tolerance;
    }
    /// Returns the current scale tolerance for the cache.
    pub fn scale_tolerance(&self) -> f32 {
        self.scale_tolerance
    }
    /// Sets the subpixel position tolerance for the cache. See the
    /// documentation for `CacheBuilder` for more information.
    ///
    /// # Panics
    ///
    /// `tolerance` is less than or equal to zero.
    pub fn set_position_tolerance(&mut self, tolerance: f32) {
        assert!(tolerance >= 0.0);
        let tolerance = tolerance.max(0.001);
        self.position_tolerance = tolerance;
    }
    /// Returns the current subpixel position tolerance for the cache.
    pub fn position_tolerance(&self) -> f32 {
        self.position_tolerance
    }
    /// Returns the cache texture dimensions assumed by the cache. For proper
    /// operation this should match the dimensions of the used GPU texture.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    /// Queue a glyph for caching by the next call to `cache_queued`. `font_id`
    /// is used to disambiguate glyphs from different fonts. The user should
    /// ensure that `font_id` is unique to the font the glyph is from.
    pub fn queue_glyph(&mut self, font_id: usize, glyph: PositionedGlyph<'font>) {
        if glyph.pixel_bounding_box().is_some() {
            self.queue.push((font_id, glyph));
        }
    }
    /// Clears the cache. Does not affect the glyph queue.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.space_end_for_start.clear();
        self.space_end_for_start.insert(0, self.height);
        self.space_start_for_end.clear();
        self.space_start_for_end.insert(self.height, 0);
        self.all_glyphs.clear();
    }
    /// Clears the glyph queue.
    pub fn clear_queue(&mut self) {
        self.queue.clear();
    }
    /// Caches the queued glyphs. If this is unsuccessful, the queue is
    /// untouched. Any glyphs cached by previous calls to this function may be
    /// removed from the cache to make room for the newly queued glyphs. Thus if
    /// you want to ensure that a glyph is in the cache, the most recently
    /// cached queue must have contained that glyph.
    ///
    /// `uploader` is the user-provided function that should perform the texture
    /// uploads to the GPU. The information provided is the rectangular region
    /// to insert the pixel data into, and the pixel data itself. This data is
    /// provided in horizontal scanline format (row major), with stride equal to
    /// the rectangle width.
    pub fn cache_queued<F: FnMut(Rect<u32>, &[u8])>(
        &mut self,
        mut uploader: F,
    ) -> Result<(), CacheWriteErr> {
        use vector;

        let mut in_use_rows = HashSet::with_capacity(self.rows.len());
        // tallest first gives better packing
        // can use 'sort_unstable' as order of equal elements is unimportant
        self.queue
            .sort_unstable_by_key(|&(_, ref glyph)| -glyph.pixel_bounding_box().unwrap().height());
        let mut queue_success = true;
        'per_glyph: for &(font_id, ref glyph) in &self.queue {
            // Check to see if it's already cached, or a close enough version is:
            // (Note that the search for "close enough" here is conservative - there may be
            // a close enough glyph that isn't found; identical glyphs however will always
            // be found)
            let p = glyph.position();
            let pfract = normalise_pixel_offset(vector(p.x.fract(), p.y.fract()));
            let font_glyph = (font_id, glyph.id());
            let spec = GlyphScaleOffset::new(glyph.scale(), pfract);

            {
                let cached = self.all_glyphs.get(&(font_id, glyph.id()));
                let lower = cached.and_then(|tree| {
                    tree.range((Unbounded, Included(&spec)))
                        .rev()
                        .next()
                        .and_then(|(l, &(lrow, _))| {
                            if spec.matches(l, self.scale_tolerance, self.position_tolerance) {
                                Some((l, lrow))
                            } else {
                                None
                            }
                        })
                });
                let upper = cached.and_then(|tree| {
                    tree.range((Included(&spec), Unbounded))
                        .next()
                        .and_then(|(u, &(urow, _))| {
                            if spec.matches(u, self.scale_tolerance, self.position_tolerance) {
                                Some((u, urow))
                            } else {
                                None
                            }
                        })
                });
                match (lower, upper) {
                    (None, None) => {} // No match
                    (None, Some((_, row))) | (Some((_, row)), None) => {
                        // just one match
                        self.rows.get_refresh(&row);
                        in_use_rows.insert(row);
                        continue 'per_glyph;
                    }
                    (Some((_, row1)), Some((_, row2))) if row1 == row2 => {
                        // two matches, but the same row
                        self.rows.get_refresh(&row1);
                        in_use_rows.insert(row1);
                        continue 'per_glyph;
                    }
                    (Some((lower, l_row)), Some((upper, u_row))) => {
                        // two definitely distinct matches
                        let l_dist = lower.match_distance(
                            &spec,
                            self.scale_tolerance,
                            self.position_tolerance,
                        );
                        let u_dist = upper.match_distance(
                            &spec,
                            self.scale_tolerance,
                            self.position_tolerance,
                        );
                        let row = if l_dist < u_dist { l_row } else { u_row };
                        self.rows.get_refresh(&row);
                        in_use_rows.insert(row);
                        continue 'per_glyph;
                    }
                }
            }
            // Not cached, so add it:
            let (width, height) = {
                let bb = glyph.pixel_bounding_box().unwrap();
                if self.pad_glyphs {
                    (bb.width() as u32 + 2, bb.height() as u32 + 2)
                } else {
                    (bb.width() as u32, bb.height() as u32)
                }
            };
            if width >= self.width || height >= self.height {
                return Result::Err(CacheWriteErr::GlyphTooLarge);
            }
            // find row to put the glyph in, most used rows first
            let mut row_top = None;
            for (top, row) in self.rows.iter().rev() {
                if row.height >= height && self.width - row.width >= width {
                    // found a spot on an existing row
                    row_top = Some(*top);
                    break;
                }
            }

            if row_top.is_none() {
                let mut gap = None;
                // See if there is space for a new row
                for (start, end) in &self.space_end_for_start {
                    if end - start >= height {
                        gap = Some((*start, *end));
                        break;
                    }
                }
                if gap.is_none() {
                    // Remove old rows until room is available
                    while !self.rows.is_empty() {
                        // check that the oldest row isn't also in use
                        if !in_use_rows.contains(self.rows.front().unwrap().0) {
                            // Remove row
                            let (top, row) = self.rows.pop_front().unwrap();

                            for g in row.glyphs {
                                if let Some(ref mut tex_info) =
                                    self.all_glyphs.get_mut(&g.font_glyph)
                                {
                                    tex_info.remove(&g.scale_offset);
                                }
                            }

                            let (mut new_start, mut new_end) = (top, top + row.height);
                            // Update the free space maps
                            if let Some(end) = self.space_end_for_start.remove(&new_end) {
                                new_end = end;
                            }
                            if let Some(start) = self.space_start_for_end.remove(&new_start) {
                                new_start = start;
                            }
                            self.space_start_for_end.insert(new_end, new_start);
                            self.space_end_for_start.insert(new_start, new_end);
                            if new_end - new_start >= height {
                                // The newly formed gap is big enough
                                gap = Some((new_start, new_end));
                                break;
                            }
                        }
                        // all rows left are in use
                        // try a clean insert of all needed glyphs
                        // if that doesn't work, fail
                        else if self.queue_retry {
                            // already trying a clean insert, don't do it again
                            return Err(CacheWriteErr::NoRoomForWholeQueue);
                        } else {
                            // signal that a retry is needed
                            queue_success = false;
                            break 'per_glyph;
                        }
                    }
                }
                let (gap_start, gap_end) = gap.unwrap();
                // fill space for new row
                let new_space_start = gap_start + height;
                self.space_end_for_start.remove(&gap_start);
                if new_space_start == gap_end {
                    self.space_start_for_end.remove(&gap_end);
                } else {
                    self.space_end_for_start.insert(new_space_start, gap_end);
                    self.space_start_for_end.insert(gap_end, new_space_start);
                }
                // add the row
                self.rows.insert(
                    gap_start,
                    Row {
                        width: 0,
                        height,
                        glyphs: Vec::new(),
                    },
                );
                row_top = Some(gap_start);
            }
            let row_top = row_top.unwrap();
            // calculate the target rect
            let row = self.rows.get_refresh(&row_top).unwrap();
            let rect = Rect {
                min: point(row.width, row_top),
                max: point(row.width + width, row_top + height),
            };
            // draw the glyph into main memory
            let mut pixels = ByteArray2d::zeros(height as usize, width as usize);
            if self.pad_glyphs {
                glyph.draw(|x, y, v| {
                    let v = (v * 255.0).round().max(0.0).min(255.0) as u8;
                    // `+ 1` accounts for top/left glyph padding
                    pixels[(y as usize + 1, x as usize + 1)] = v;
                });
            } else {
                glyph.draw(|x, y, v| {
                    let v = (v * 255.0).round().max(0.0).min(255.0) as u8;
                    pixels[(y as usize, x as usize)] = v;
                });
            }
            // transfer
            uploader(rect, pixels.as_slice());
            // add the glyph to the row
            row.glyphs.push(GlyphTexInfo {
                font_glyph,
                scale_offset: spec,
                tex_coords: rect,
            });
            row.width += width;
            in_use_rows.insert(row_top);

            self.all_glyphs
                .entry(font_glyph)
                .or_insert_with(BTreeMap::new)
                .insert(spec, (row_top, row.glyphs.len() as u32 - 1));
        }
        if queue_success {
            self.queue.clear();
            Ok(())
        } else {
            // clear the cache then try again
            self.clear();
            self.queue_retry = true;
            let result = self.cache_queued(uploader);
            self.queue_retry = false;
            result
        }
    }

    /// Retrieves the (floating point) texture coordinates of the quad for a
    /// glyph in the cache, as well as the pixel-space (integer) coordinates
    /// that this region should be drawn at. In the majority of cases these
    /// pixel-space coordinates should be identical to the bounding box of the
    /// input glyph. They only differ if the cache has returned a substitute
    /// glyph that is deemed close enough to the requested glyph as specified by
    /// the cache tolerance parameters.
    ///
    /// A sucessful result is `Some` if the glyph is not an empty glyph (no
    /// shape, and thus no rect to return).
    ///
    /// Ensure that `font_id` matches the `font_id` that was passed to
    /// `queue_glyph` with this `glyph`.
    pub fn rect_for<'a>(
        &'a self,
        font_id: usize,
        glyph: &PositionedGlyph,
    ) -> Result<Option<TextureCoords>, CacheReadErr> {
        use vector;
        let glyph_bb = match glyph.pixel_bounding_box() {
            Some(bb) => bb,
            None => return Ok(None),
        };
        let target_position = glyph.position();
        let target_offset =
            normalise_pixel_offset(vector(target_position.x.fract(), target_position.y.fract()));

        let font_glyph = (font_id, glyph.id());
        let target_spec = GlyphScaleOffset::new(glyph.scale(), target_offset);

        let glyphs = self.all_glyphs.get(&font_glyph);
        let (lower, upper) = glyphs
            .map(|glyphs| {
                let mut left_range = glyphs.range((Unbounded, Included(&target_spec))).rev();
                let mut right_range = glyphs.range((Included(&target_spec), Unbounded));

                let mut left = left_range.next().map(|(s, &(r, i))| (s, r, i));
                let mut right = right_range.next().map(|(s, &(r, i))| (s, r, i));

                while left.is_some() || right.is_some() {
                    left = left.and_then(|(spec, row, index)| {
                        if spec.matches(&target_spec, self.scale_tolerance, self.position_tolerance)
                        {
                            Some((spec, row, index))
                        } else {
                            None
                        }
                    });
                    right = right.and_then(|(spec, row, index)| {
                        if spec.matches(&target_spec, self.scale_tolerance, self.position_tolerance)
                        {
                            Some((spec, row, index))
                        } else {
                            None
                        }
                    });

                    if left.is_none() && right.is_none() {
                        // continue searching for a match
                        left = left_range.next().map(|(s, &(r, i))| (s, r, i));
                        right = right_range.next().map(|(s, &(r, i))| (s, r, i));
                    } else {
                        break;
                    }
                }
                (left, right)
            })
            .unwrap_or((None, None));

        let (tex_width, tex_height) = (self.width as f32, self.height as f32);
        let (match_spec, row, index) = match (lower, upper) {
            (None, None) => return Err(CacheReadErr::GlyphNotCached),
            (Some(match_), None) | (None, Some(match_)) => match_, // one match
            (Some((lmatch_spec, lrow, lindex)), Some((umatch_spec, urow, uindex))) => {
                if lrow == urow && lindex == uindex {
                    // both matches are really the same one, and match the input
                    let mut tex_rect = self.rows[&lrow].glyphs[lindex as usize].tex_coords;
                    if self.pad_glyphs {
                        tex_rect = tex_rect.unpadded();
                    }
                    let uv_rect = Rect {
                        min: point(
                            tex_rect.min.x as f32 / tex_width,
                            tex_rect.min.y as f32 / tex_height,
                        ),
                        max: point(
                            tex_rect.max.x as f32 / tex_width,
                            tex_rect.max.y as f32 / tex_height,
                        ),
                    };
                    return Ok(Some((uv_rect, glyph_bb)));
                } else {
                    // Two close-enough matches. Figure out which is closest.
                    let l_measure = lmatch_spec.match_distance(
                        &target_spec,
                        self.scale_tolerance,
                        self.position_tolerance,
                    );
                    let u_measure = umatch_spec.match_distance(
                        &target_spec,
                        self.scale_tolerance,
                        self.position_tolerance,
                    );
                    if l_measure < u_measure {
                        (lmatch_spec, lrow, lindex)
                    } else {
                        (umatch_spec, urow, uindex)
                    }
                }
            }
        };
        let mut tex_rect = self.rows[&row].glyphs[index as usize].tex_coords;
        if self.pad_glyphs {
            tex_rect = tex_rect.unpadded();
        }
        let uv_rect = Rect {
            min: point(
                tex_rect.min.x as f32 / tex_width,
                tex_rect.min.y as f32 / tex_height,
            ),
            max: point(
                tex_rect.max.x as f32 / tex_width,
                tex_rect.max.y as f32 / tex_height,
            ),
        };
        let local_bb = glyph
            .unpositioned()
            .clone()
            .positioned(point(0.0, 0.0) + match_spec.offset())
            .pixel_bounding_box()
            .unwrap();
        let min_from_origin = point(local_bb.min.x as f32, local_bb.min.y as f32)
            - (point(0.0, 0.0) + match_spec.offset());
        let ideal_min = min_from_origin + target_position;
        let min = point(ideal_min.x.round() as i32, ideal_min.y.round() as i32);
        let bb_offset = min - local_bb.min;
        let bb = Rect {
            min,
            max: local_bb.max + bb_offset,
        };
        Ok(Some((uv_rect, bb)))
    }
}

#[cfg(test)]
#[test]
fn cache_test() {
    use FontCollection;
    use Scale;
    let font_data = include_bytes!("../fonts/wqy-microhei/WenQuanYiMicroHei.ttf");
    let font = FontCollection::from_bytes(font_data as &[u8])
        .unwrap()
        .into_font()
        .unwrap();
    let mut cache = Cache::new(32, 32, 0.1, 0.1);
    let strings = [
        ("Hello World!", 15.0),
        ("Hello World!", 14.0),
        ("Hello World!", 10.0),
        ("Hello World!", 15.0),
        ("Hello World!", 14.0),
        ("Hello World!", 10.0),
    ];
    for &(string, scale) in &strings {
        println!("Caching {:?}", (string, scale));
        for glyph in font.layout(string, Scale::uniform(scale), point(0.0, 0.0)) {
            cache.queue_glyph(0, glyph);
        }
        cache.cache_queued(|_, _| {}).unwrap();
    }
}

#[cfg(test)]
#[test]
fn need_to_check_whole_cache() {
    use FontCollection;
    use Scale;
    let font_data = include_bytes!("../fonts/wqy-microhei/WenQuanYiMicroHei.ttf");
    let font = FontCollection::from_bytes(font_data as &[u8])
        .unwrap()
        .into_font()
        .unwrap();

    let glyph = font.glyph('l');

    let small = glyph.clone().scaled(Scale::uniform(10.0));
    let large = glyph.clone().scaled(Scale::uniform(10.05));

    let small_left = small.clone().positioned(point(0.0, 0.0));
    let large_left = large.clone().positioned(point(0.0, 0.0));
    let large_right = large.clone().positioned(point(-0.2, 0.0));

    let mut cache = Cache::new(32, 32, 0.1, 0.1);

    cache.queue_glyph(0, small_left.clone());
    // Next line is noop since it's within the scale tolerance of small_left:
    cache.queue_glyph(0, large_left.clone());
    cache.queue_glyph(0, large_right.clone());

    cache.cache_queued(|_, _| {}).unwrap();

    cache.rect_for(0, &small_left).unwrap();
    cache.rect_for(0, &large_left).unwrap();
    cache.rect_for(0, &large_right).unwrap();
}

#[cfg(feature = "bench")]
#[cfg(test)]
mod cache_bench_tests {
    use super::*;
    use {point, Font, Scale};

    lazy_static! {
        static ref FONTS: Vec<Font<'static>> = vec![
            include_bytes!("../fonts/wqy-microhei/WenQuanYiMicroHei.ttf") as &[u8],
            include_bytes!("../fonts/dejavu/DejaVuSansMono.ttf") as &[u8],
            include_bytes!("../fonts/opensans/OpenSans-Italic.ttf") as &[u8],
        ].into_iter()
            .map(|bytes| Font::from_bytes(bytes).unwrap())
            .collect();
    }

    const TEST_STR: &str = include_str!("../tests/lipsum.txt");

    /// Reproduces Err(GlyphNotCached) issue & serves as a general purpose
    /// cache benchmark
    #[bench]
    fn cache_bench_tolerance_p1(b: &mut ::test::Bencher) {
        let font_id = 0;
        let glyphs = test_glyphs(&FONTS[font_id], TEST_STR);
        let mut cache = CacheBuilder{
            width: 1024,
            height: 1024,
            scale_tolerance: 0.1,
            position_tolerance: 0.1,
            ..CacheBuilder::default()
        }.build();

        b.iter(|| {
            for glyph in &glyphs {
                cache.queue_glyph(font_id, glyph.clone());
            }

            cache.cache_queued(|_, _| {}).expect("cache_queued");

            for (index, glyph) in glyphs.iter().enumerate() {
                let rect = cache.rect_for(font_id, glyph);
                assert!(
                    rect.is_ok(),
                    "Gpu cache rect lookup failed ({:?}) for glyph index {}, id {}",
                    rect,
                    index,
                    glyph.id().0
                );
            }
        });
    }

    #[bench]
    fn cache_bench_tolerance_1(b: &mut ::test::Bencher) {
        let font_id = 0;
        let glyphs = test_glyphs(&FONTS[font_id], TEST_STR);
        let mut cache = CacheBuilder{
            width: 1024,
            height: 1024,
            scale_tolerance: 0.1,
            position_tolerance: 1.0,
            ..CacheBuilder::default()
        }.build();

        b.iter(|| {
            for glyph in &glyphs {
                cache.queue_glyph(font_id, glyph.clone());
            }

            cache.cache_queued(|_, _| {}).expect("cache_queued");

            for (index, glyph) in glyphs.iter().enumerate() {
                let rect = cache.rect_for(font_id, glyph);
                assert!(
                    rect.is_ok(),
                    "Gpu cache rect lookup failed ({:?}) for glyph index {}, id {}",
                    rect,
                    index,
                    glyph.id().0
                );
            }
        });
    }

    #[bench]
    fn cache_bench_tolerance_p1_multifont(b: &mut ::test::Bencher) {
        let up_to_index = TEST_STR
            .char_indices()
            .nth(TEST_STR.chars().count() / FONTS.len())
            .unwrap()
            .0;
        // Use a smaller amount of the test string, to offset the extra font-glyph
        // bench load
        let string = &TEST_STR[..up_to_index];

        let font_glyphs: Vec<_> = FONTS
            .iter()
            .enumerate()
            .map(|(id, font)| (id, test_glyphs(font, string)))
            .collect();
        let mut cache = CacheBuilder{
            width: 1024,
            height: 1024,
            scale_tolerance: 0.1,
            position_tolerance: 0.1,
            ..CacheBuilder::default()
        }.build();

        b.iter(|| {
            for &(font_id, ref glyphs) in &font_glyphs {
                for glyph in glyphs {
                    cache.queue_glyph(font_id, glyph.clone());
                }
            }

            cache.cache_queued(|_, _| {}).expect("cache_queued");

            for &(font_id, ref glyphs) in &font_glyphs {
                for (index, glyph) in glyphs.iter().enumerate() {
                    let rect = cache.rect_for(font_id, glyph);
                    assert!(
                        rect.is_ok(),
                        "Gpu cache rect lookup failed ({:?}) for font {} glyph index {}, id {}",
                        rect,
                        font_id,
                        index,
                        glyph.id().0
                    );
                }
            }
        });
    }

    fn test_glyphs<'a>(font: &Font<'a>, string: &str) -> Vec<PositionedGlyph<'a>> {
        let mut glyphs = vec![];
        // Set of scales, found through brute force, to reproduce GlyphNotCached issue
        // Cache settings also affect this, it occurs when position_tolerance is < 1.0
        for scale in &[25_f32, 24.5, 25.01, 24.7, 24.99] {
            for glyph in layout_paragraph(font, Scale::uniform(*scale), 500, string) {
                glyphs.push(glyph);
            }
        }
        glyphs
    }

    fn layout_paragraph<'a>(
        font: &Font<'a>,
        scale: Scale,
        width: u32,
        text: &str,
    ) -> Vec<PositionedGlyph<'a>> {
        use unicode_normalization::UnicodeNormalization;
        let mut result = Vec::new();
        let v_metrics = font.v_metrics(scale);
        let advance_height = v_metrics.ascent - v_metrics.descent + v_metrics.line_gap;
        let mut caret = point(0.0, v_metrics.ascent);
        let mut last_glyph_id = None;
        for c in text.nfc() {
            if c.is_control() {
                match c {
                    '\n' => caret = point(0.0, caret.y + advance_height),
                    _ => {}
                }
                continue;
            }
            let base_glyph = font.glyph(c);
            if let Some(id) = last_glyph_id.take() {
                caret.x += font.pair_kerning(scale, id, base_glyph.id());
            }
            last_glyph_id = Some(base_glyph.id());
            let mut glyph = base_glyph.scaled(scale).positioned(caret);
            if let Some(bb) = glyph.pixel_bounding_box() {
                if bb.max.x > width as i32 {
                    caret = point(0.0, caret.y + advance_height);
                    glyph = glyph.into_unpositioned().positioned(caret);
                    last_glyph_id = None;
                }
            }
            caret.x += glyph.unpositioned().h_metrics().advance_width;
            result.push(glyph);
        }
        result
    }
}
