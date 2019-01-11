// pathfinder/utils/tile-svg/main.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![allow(clippy::float_cmp)]

#[macro_use]
extern crate bitflags;

#[cfg(test)]
extern crate quickcheck;
#[cfg(test)]
extern crate rand;

use arrayvec::ArrayVec;
use byteorder::{LittleEndian, WriteBytesExt};
use clap::{App, Arg};
use euclid::{Point2D, Rect, Size2D};
use fixedbitset::FixedBitSet;
use hashbrown::HashMap;
use jemallocator;
use lyon_path::PathEvent;
use lyon_path::iterator::PathIter;
use pathfinder_geometry::line_segment::{LineSegmentF32, LineSegmentU4, LineSegmentU8};
use pathfinder_geometry::point::Point2DF32;
use pathfinder_geometry::stroke::{StrokeStyle, StrokeToFillIter};
use pathfinder_geometry::util;
use rayon::ThreadPoolBuilder;
use rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use simdeez::Simd;
use simdeez::overloads::I32x4_41;
use simdeez::sse41::Sse41;
use std::arch::x86_64;
use std::cmp::Ordering;
use std::fmt::{self, Debug, Formatter};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::iter;
use std::mem;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};
use std::u16;
use std::u32;
use svgtypes::Color as SvgColor;
use usvg::{Node, NodeExt, NodeKind, Options as UsvgOptions, Paint as UsvgPaint};
use usvg::{PathSegment as UsvgPathSegment, Rect as UsvgRect, Transform as UsvgTransform, Tree};

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

// TODO(pcwalton): Make this configurable.
const SCALE_FACTOR: f32 = 1.0;

// TODO(pcwalton): Make this configurable.
const FLATTENING_TOLERANCE: f32 = 0.1;

const HAIRLINE_STROKE_WIDTH: f32 = 0.1;

const MAX_FILLS_PER_BATCH: usize = 0x0002_0000;
const MAX_MASKS_PER_BATCH: u16 = 0xffff;

fn main() {
    let matches =
        App::new("tile-svg").arg(Arg::with_name("runs").short("r")
                                                       .long("runs")
                                                       .value_name("COUNT")
                                                       .takes_value(true)
                                                       .help("Run a benchmark with COUNT runs"))
                            .arg(Arg::with_name("jobs").short("j")
                                                       .long("jobs")
                                                       .value_name("THREADS")
                                                       .takes_value(true)
                                                       .help("Number of threads to use"))
                            .arg(Arg::with_name("INPUT").help("Path to the SVG file to render")
                                                        .required(true)
                                                        .index(1))
                            .arg(Arg::with_name("OUTPUT").help("Path to the output PF3 data")
                                                         .required(false)
                                                         .index(2))
                            .get_matches();
    let runs: usize = match matches.value_of("runs") {
        Some(runs) => runs.parse().unwrap(),
        None => 1,
    };
    let jobs: Option<usize> = matches.value_of("jobs").map(|string| string.parse().unwrap());
    let input_path = PathBuf::from(matches.value_of("INPUT").unwrap());
    let output_path = matches.value_of("OUTPUT").map(PathBuf::from);

    // Set up Rayon.
    let mut thread_pool_builder = ThreadPoolBuilder::new();
    if let Some(jobs) = jobs {
        thread_pool_builder = thread_pool_builder.num_threads(jobs);
    }
    thread_pool_builder.build_global().unwrap();

    // Build scene.
    let usvg = Tree::from_file(&input_path, &UsvgOptions::default()).unwrap();
    let scene = Scene::from_tree(usvg);

    println!("Scene bounds: {:?} View box: {:?}", scene.bounds, scene.view_box);
    println!("{} objects, {} paints", scene.objects.len(), scene.paints.len());

    let (mut elapsed_object_build_time, mut elapsed_scene_build_time) = (0.0, 0.0);

    let mut built_scene = BuiltScene::new(&scene.view_box);
    for _ in 0..runs {
        let z_buffer = ZBuffer::new(&scene.view_box);

        let start_time = Instant::now();
        let built_objects = match jobs {
            Some(1) => scene.build_objects_sequentially(&z_buffer),
            _ => scene.build_objects(&z_buffer),
        };
        elapsed_object_build_time += duration_to_ms(&(Instant::now() - start_time));

        let start_time = Instant::now();
        built_scene = BuiltScene::new(&scene.view_box);
        built_scene.shaders = scene.build_shaders();
        let mut scene_builder = SceneBuilder::new(built_objects, z_buffer, &scene.view_box);
        built_scene.solid_tiles = scene_builder.build_solid_tiles();
        while let Some(batch) = scene_builder.build_batch() {
            built_scene.batches.push(batch);
        }
        elapsed_scene_build_time += duration_to_ms(&(Instant::now() - start_time));
    }

    elapsed_object_build_time /= runs as f64;
    elapsed_scene_build_time /= runs as f64;
    let total_elapsed_time = elapsed_object_build_time + elapsed_scene_build_time;

    println!("{:.3}ms ({:.3}ms objects, {:.3}ms scene) elapsed",
             total_elapsed_time,
             elapsed_object_build_time,
             elapsed_scene_build_time);

    println!("{} solid tiles", built_scene.solid_tiles.len());
    for (batch_index, batch) in built_scene.batches.iter().enumerate() {
        println!("Batch {}: {} fills, {} mask tiles",
                 batch_index,
                 batch.fills.len(),
                 batch.mask_tiles.len());
    }

    if let Some(output_path) = output_path {
        built_scene.write(&mut BufWriter::new(File::create(output_path).unwrap())).unwrap();
    }
}

fn duration_to_ms(duration: &Duration) -> f64 {
    duration.as_secs() as f64 * 1000.0 + f64::from(duration.subsec_micros()) / 1000.0
}

#[derive(Debug)]
struct Scene {
    objects: Vec<PathObject>,
    paints: Vec<Paint>,
    paint_cache: HashMap<Paint, PaintId>,
    bounds: Rect<f32>,
    view_box: Rect<f32>,
}

#[derive(Debug)]
struct PathObject {
    outline: Outline,
    paint: PaintId,
    name: String,
    kind: PathObjectKind,
}

#[derive(Clone, Copy, Debug)]
pub enum PathObjectKind {
    Fill,
    Stroke,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Paint {
    color: ColorU,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct PaintId(u16);

impl Scene {
    fn new() -> Scene {
        Scene {
            objects: vec![],
            paints: vec![],
            paint_cache: HashMap::new(),
            bounds: Rect::zero(),
            view_box: Rect::zero(),
        }
    }

    fn from_tree(tree: Tree) -> Scene {
        let global_transform = Transform2DF32::from_scale(&Point2DF32::splat(SCALE_FACTOR));

        let mut scene = Scene::new();

        let root = &tree.root();
        match *root.borrow() {
            NodeKind::Svg(ref svg) => {
                scene.view_box = usvg_rect_to_euclid_rect(&svg.view_box.rect);
                for kid in root.children() {
                    process_node(&mut scene, &kid, &global_transform);
                }
            }
            _ => unreachable!(),
        };

        // FIXME(pcwalton): This is needed to avoid stack exhaustion in debug builds when
        // recursively dropping reference counts on very large SVGs. :(
        mem::forget(tree);

        return scene;

        fn process_node(scene: &mut Scene, node: &Node, transform: &Transform2DF32) {
            let node_transform = usvg_transform_to_transform_2d(&node.transform());
            let transform = transform.pre_mul(&node_transform);

            match *node.borrow() {
                NodeKind::Group(_) => {
                    for kid in node.children() {
                        process_node(scene, &kid, &transform)
                    }
                }
                NodeKind::Path(ref path) => {
                    if let Some(ref fill) = path.fill {
                        let style = scene.push_paint(&Paint::from_svg_paint(&fill.paint));

                        let path = UsvgPathToSegments::new(path.segments.iter().cloned());
                        let path = PathTransformingIter::new(path, &transform);
                        let path = MonotonicConversionIter::new(path);
                        let outline = Outline::from_segments(path);

                        scene.bounds = scene.bounds.union(&outline.bounds);
                        scene.objects.push(PathObject::new(outline,
                                                           style,
                                                           node.id().to_string(),
                                                           PathObjectKind::Fill));
                    }

                    if let Some(ref stroke) = path.stroke {
                        let style = scene.push_paint(&Paint::from_svg_paint(&stroke.paint));
                        let stroke_width = f32::max(stroke.width.value() as f32,
                                                    HAIRLINE_STROKE_WIDTH);

                        let path = UsvgPathToSegments::new(path.segments.iter().cloned());
                        let path = SegmentsToPathEvents::new(path);
                        let path = PathIter::new(path);
                        let path = StrokeToFillIter::new(path, StrokeStyle::new(stroke_width));
                        let path = PathEventsToSegments::new(path);
                        let path = PathTransformingIter::new(path, &transform);
                        let path = MonotonicConversionIter::new(path);
                        let outline = Outline::from_segments(path);

                        scene.bounds = scene.bounds.union(&outline.bounds);
                        scene.objects.push(PathObject::new(outline,
                                                           style,
                                                           node.id().to_string(),
                                                           PathObjectKind::Stroke));
                    }
                }
                _ => {
                    // TODO(pcwalton): Handle these by punting to WebRender.
                }
            }
        }
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn push_paint(&mut self, paint: &Paint) -> PaintId {
        if let Some(paint_id) = self.paint_cache.get(paint) {
            return *paint_id
        }

        let paint_id = PaintId(self.paints.len() as u16);
        self.paint_cache.insert(*paint, paint_id);
        self.paints.push(*paint);
        paint_id
    }

    fn build_shaders(&self) -> Vec<ObjectShader> {
        self.paints.iter().map(|paint| ObjectShader { fill_color: paint.color }).collect()
    }

    fn build_objects_sequentially(&self, z_buffer: &ZBuffer) -> Vec<BuiltObject> {
        self.objects.iter().enumerate().map(|(object_index, object)| {
            let mut tiler = Tiler::new(&object.outline,
                                       &self.view_box,
                                       object_index as u16,
                                       ShaderId(object.paint.0),
                                       z_buffer);
            tiler.generate_tiles();
            tiler.built_object
        }).collect()
    }

    fn build_objects(&self, z_buffer: &ZBuffer) -> Vec<BuiltObject> {
        self.objects.par_iter().enumerate().map(|(object_index, object)| {
            let mut tiler = Tiler::new(&object.outline,
                                       &self.view_box,
                                       object_index as u16,
                                       ShaderId(object.paint.0),
                                       z_buffer);
            tiler.generate_tiles();
            tiler.built_object
        }).collect()
    }
}

impl PathObject {
    fn new(outline: Outline, paint: PaintId, name: String, kind: PathObjectKind) -> PathObject {
        PathObject { outline, paint, name, kind }
    }
}

// Outlines

#[derive(Debug)]
struct Outline {
    contours: Vec<Contour>,
    bounds: Rect<f32>,
}

struct Contour {
    points: Vec<Point2DF32>,
    flags: Vec<PointFlags>,
}

bitflags! {
    struct PointFlags: u8 {
        const CONTROL_POINT_0 = 0x01;
        const CONTROL_POINT_1 = 0x02;
    }
}

impl Outline {
    fn new() -> Outline {
        Outline {
            contours: vec![],
            bounds: Rect::zero(),
        }
    }

    fn from_segments<I>(segments: I) -> Outline where I: Iterator<Item = Segment> {
        let mut outline = Outline::new();
        let mut current_contour = Contour::new();
        let mut bounding_points = None;

        for segment in segments {
            if segment.flags.contains(SegmentFlags::FIRST_IN_SUBPATH) {
                if !current_contour.is_empty() {
                    outline.contours.push(mem::replace(&mut current_contour, Contour::new()));
                }
                current_contour.push_point(segment.baseline.from(),
                                           PointFlags::empty(),
                                           &mut bounding_points);
            }

            if segment.flags.contains(SegmentFlags::CLOSES_SUBPATH) {
                if !current_contour.is_empty() {
                    outline.contours.push(mem::replace(&mut current_contour, Contour::new()));
                }
                continue;
            }

            if segment.is_none() {
                continue;
            }

            if !segment.is_line() {
                current_contour.push_point(segment.ctrl.from(),
                                           PointFlags::CONTROL_POINT_0,
                                           &mut bounding_points);
                if !segment.is_quadratic() {
                    current_contour.push_point(segment.ctrl.to(),
                                               PointFlags::CONTROL_POINT_1,
                                               &mut bounding_points);
                }
            }

            current_contour.push_point(segment.baseline.to(),
                                       PointFlags::empty(),
                                       &mut bounding_points);
        }

        if !current_contour.is_empty() {
            outline.contours.push(current_contour)
        }

        if let Some((upper_left, lower_right)) = bounding_points {
            outline.bounds = Rect::from_points([
                upper_left.as_euclid(),
                lower_right.as_euclid(),
            ].iter())
        }

        outline
    }
}

impl Contour {
    fn new() -> Contour {
        Contour { points: vec![], flags: vec![] }
    }

    fn iter(&self) -> ContourIter {
        ContourIter { contour: self, index: 0 }
    }

    fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    fn len(&self) -> u32 {
        self.points.len() as u32
    }

    fn position_of(&self, index: u32) -> Point2DF32 {
        self.points[index as usize]
    }

    // TODO(pcwalton): Pack both min and max into a single SIMD register?
    fn push_point(&mut self,
                  point: Point2DF32,
                  flags: PointFlags,
                  bounding_points: &mut Option<(Point2DF32, Point2DF32)>) {
        self.points.push(point);
        self.flags.push(flags);

        match *bounding_points {
            Some((ref mut upper_left, ref mut lower_right)) => {
                *upper_left = upper_left.min(point);
                *lower_right = lower_right.max(point);
            }
            None => *bounding_points = Some((point, point)),
        }
    }

    fn segment_after(&self, point_index: u32) -> Segment {
        debug_assert!(self.point_is_endpoint(point_index));

        let mut segment = Segment::none();
        segment.baseline.set_from(&self.position_of(point_index));

        let point1_index = self.add_to_point_index(point_index, 1);
        if self.point_is_endpoint(point1_index) {
            segment.baseline.set_to(&self.position_of(point1_index));
            segment.kind = SegmentKind::Line;
        } else {
            segment.ctrl.set_from(&self.position_of(point1_index));

            let point2_index = self.add_to_point_index(point_index, 2);
            if self.point_is_endpoint(point2_index) {
                segment.baseline.set_to(&self.position_of(point2_index));
                segment.kind = SegmentKind::Quadratic;
            } else {
                segment.ctrl.set_to(&self.position_of(point2_index));
                segment.kind = SegmentKind::Cubic;

                let point3_index = self.add_to_point_index(point_index, 3);
                segment.baseline.set_to(&self.position_of(point3_index));
            }
        }

        segment
    }

    fn point_is_endpoint(&self, point_index: u32) -> bool {
        !self.flags[point_index as usize].intersects(PointFlags::CONTROL_POINT_0 |
                                                     PointFlags::CONTROL_POINT_1)
    }

    fn add_to_point_index(&self, point_index: u32, addend: u32) -> u32 {
        let (index, limit) = (point_index + addend, self.len());
        if index >= limit {
            index - limit
        } else {
            index
        }
    }

    fn point_is_logically_above(&self, a: u32, b: u32) -> bool {
        let (a_y, b_y) = (self.points[a as usize].y(), self.points[b as usize].y());
        a_y < b_y || (a_y == b_y && a < b)
    }

    fn prev_endpoint_index_of(&self, mut point_index: u32) -> u32 {
        loop {
            point_index = self.prev_point_index_of(point_index);
            if self.point_is_endpoint(point_index) {
                return point_index
            }
        }
    }

    fn next_endpoint_index_of(&self, mut point_index: u32) -> u32 {
        loop {
            point_index = self.next_point_index_of(point_index);
            if self.point_is_endpoint(point_index) {
                return point_index
            }
        }
    }

    fn prev_point_index_of(&self, point_index: u32) -> u32 {
        if point_index == 0 {
            self.len() - 1
        } else {
            point_index - 1
        }
    }

    fn next_point_index_of(&self, point_index: u32) -> u32 {
        if point_index == self.len() - 1 {
            0
        } else {
            point_index + 1
        }
    }
}

impl Debug for Contour {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        formatter.write_str("[")?;
        if formatter.alternate() {
            formatter.write_str("\n")?
        }
        for (index, segment) in self.iter().enumerate() {
            if index > 0 {
                formatter.write_str(" ")?;
            }
            if formatter.alternate() {
                formatter.write_str("\n    ")?;
            }
            write_path_event(formatter, &segment)?;
        }
        if formatter.alternate() {
            formatter.write_str("\n")?
        }
        formatter.write_str("]")?;

        return Ok(());

        fn write_path_event(formatter: &mut Formatter, path_event: &PathEvent) -> fmt::Result {
            match *path_event {
                PathEvent::Arc(..) => {
                    // TODO(pcwalton)
                    formatter.write_str("TODO: arcs")?;
                }
                PathEvent::Close => formatter.write_str("z")?,
                PathEvent::MoveTo(to) => {
                    formatter.write_str("M")?;
                    write_point(formatter, to)?;
                }
                PathEvent::LineTo(to) => {
                    formatter.write_str("L")?;
                    write_point(formatter, to)?;
                }
                PathEvent::QuadraticTo(ctrl, to) => {
                    formatter.write_str("Q")?;
                    write_point(formatter, ctrl)?;
                    write_point(formatter, to)?;
                }
                PathEvent::CubicTo(ctrl0, ctrl1, to) => {
                    formatter.write_str("C")?;
                    write_point(formatter, ctrl0)?;
                    write_point(formatter, ctrl1)?;
                    write_point(formatter, to)?;
                }
            }
            Ok(())
        }

        fn write_point(formatter: &mut Formatter, point: Point2D<f32>) -> fmt::Result {
            write!(formatter, " {},{}", point.x, point.y)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct PointIndex(u32);

impl PointIndex {
    fn new(contour: u32, point: u32) -> PointIndex {
        debug_assert!(contour <= 0xfff);
        debug_assert!(point <= 0x000f_ffff);
        PointIndex((contour << 20) | point)
    }

    fn contour(self) -> u32 {
        self.0 >> 20
    }

    fn point(self) -> u32 {
        self.0 & 0x000f_ffff
    }
}

struct ContourIter<'a> {
    contour: &'a Contour,
    index: u32,
}

impl<'a> Iterator for ContourIter<'a> {
    type Item = PathEvent;

    fn next(&mut self) -> Option<PathEvent> {
        let contour = self.contour;
        if self.index == contour.len() + 1 {
            return None
        }
        if self.index == contour.len() {
            self.index += 1;
            return Some(PathEvent::Close)
        }

        let point0_index = self.index;
        let point0 = contour.position_of(point0_index);
        self.index += 1;
        if point0_index == 0 {
            return Some(PathEvent::MoveTo(point0.as_euclid()))
        }
        if contour.point_is_endpoint(point0_index) {
            return Some(PathEvent::LineTo(point0.as_euclid()))
        }

        let point1_index = self.index;
        let point1 = contour.position_of(point1_index);
        self.index += 1;
        if contour.point_is_endpoint(point1_index) {
            return Some(PathEvent::QuadraticTo(point0.as_euclid(), point1.as_euclid()))
        }

        let point2_index = self.index;
        let point2 = contour.position_of(point2_index);
        self.index += 1;
        debug_assert!(contour.point_is_endpoint(point2_index));
        Some(PathEvent::CubicTo(point0.as_euclid(), point1.as_euclid(), point2.as_euclid()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Segment {
    baseline: LineSegmentF32,
    ctrl: LineSegmentF32,
    kind: SegmentKind,
    flags: SegmentFlags,
}

impl Segment {
    fn none() -> Segment {
        Segment {
            baseline: LineSegmentF32::default(),
            ctrl: LineSegmentF32::default(),
            kind: SegmentKind::None,
            flags: SegmentFlags::empty(),
        }
    }

    fn line(line: &LineSegmentF32) -> Segment {
        Segment {
            baseline: *line,
            ctrl: LineSegmentF32::default(),
            kind: SegmentKind::Line,
            flags: SegmentFlags::empty(),
        }
    }

    fn quadratic(baseline: &LineSegmentF32, ctrl: &Point2DF32) -> Segment {
        Segment {
            baseline: *baseline,
            ctrl: LineSegmentF32::new(ctrl, &Point2DF32::default()),
            kind: SegmentKind::Cubic,
            flags: SegmentFlags::empty(),
        }
    }

    fn cubic(baseline: &LineSegmentF32, ctrl: &LineSegmentF32) -> Segment {
        Segment {
            baseline: *baseline,
            ctrl: *ctrl,
            kind: SegmentKind::Cubic,
            flags: SegmentFlags::empty(),
        }
    }

    fn as_line_segment(&self) -> LineSegmentF32 {
        debug_assert!(self.is_line());
        self.baseline
    }

    fn is_none(&self)      -> bool { self.kind == SegmentKind::None      }
    fn is_line(&self)      -> bool { self.kind == SegmentKind::Line      }
    fn is_quadratic(&self) -> bool { self.kind == SegmentKind::Quadratic }
    fn is_cubic(&self)     -> bool { self.kind == SegmentKind::Cubic     }

    fn as_cubic_segment(&self) -> CubicSegment {
        debug_assert!(self.is_cubic());
        CubicSegment(self)
    }

    // FIXME(pcwalton): We should basically never use this function.
    // FIXME(pcwalton): Handle lines!
    fn to_cubic(&self) -> Segment {
        if self.is_cubic() {
            return *self;
        }

        let mut new_segment = *self;
        let p1_2 = self.ctrl.from() + self.ctrl.from();
        new_segment.ctrl = LineSegmentF32::new(&(self.baseline.from() + p1_2),
                                               &(p1_2 + self.baseline.to())).scale(1.0 / 3.0);
        new_segment
    }

    fn reversed(&self) -> Segment {
        Segment {
            baseline: self.baseline.reversed(),
            ctrl: if self.is_quadratic() { self.ctrl } else { self.ctrl.reversed() },
            kind: self.kind,
            flags: self.flags,
        }
    }

    // Reverses if necessary so that the from point is above the to point. Calling this method
    // again will undo the transformation.
    fn orient(&self, y_winding: i32) -> Segment {
        if y_winding >= 0 {
            *self
        } else {
            self.reversed()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(u8)]
enum SegmentKind {
    None,
    Line,
    Quadratic,
    Cubic,
}

bitflags! {
    struct SegmentFlags: u8 {
        const FIRST_IN_SUBPATH = 0x01;
        const CLOSES_SUBPATH = 0x02;
    }
}

#[derive(Clone, Copy, Debug)]
struct CubicSegment<'s>(&'s Segment);

impl<'s> CubicSegment<'s> {
    fn flatten_once(self) -> Option<Segment> {
        let s2inv;
        unsafe {
            let (baseline, ctrl) = (self.0.baseline.0, self.0.ctrl.0);
            let from_from = Sse41::shuffle_ps(baseline, baseline, 0b0100_0100);

            let v0102 = Sse41::sub_ps(ctrl, from_from);

            //      v01.x   v01.y   v02.x v02.y
            //    * v01.x   v01.y   v01.y v01.x
            //    -------------------------
            //      v01.x^2 v01.y^2 ad    bc
            //         |       |     |     |
            //         +-------+     +-----+
            //             +            -
            //         v01 len^2   determinant
            let products = Sse41::mul_ps(v0102, Sse41::shuffle_ps(v0102, v0102, 0b0001_0100));

            let det = products[2] - products[3];
            if det == 0.0 {
                return None;
            }

            s2inv = (products[0] + products[1]).sqrt() / det;
        }

        let t = 2.0 * ((FLATTENING_TOLERANCE / 3.0) * s2inv.abs()).sqrt();
        if t >= 1.0 - EPSILON || t == 0.0 {
            return None;
        }

        return Some(self.split_after(t));

        const EPSILON: f32 = 0.005;
    }

    fn split(self, t: f32) -> (Segment, Segment) {
        unsafe {
            let tttt = Sse41::set1_ps(t);

            let p0p3 = self.0.baseline.0;
            let p1p2 = self.0.ctrl.0;
            let p0p1 = assemble(&p0p3, &p1p2, 0, 0);

            // p01 = lerp(p0, p1, t), p12 = lerp(p1, p2, t), p23 = lerp(p2, p3, t)
            let p01p12 = Sse41::add_ps(p0p1, Sse41::mul_ps(tttt, Sse41::sub_ps(p1p2, p0p1)));
            let pxxp23 = Sse41::add_ps(p1p2, Sse41::mul_ps(tttt, Sse41::sub_ps(p0p3, p1p2)));

            let p12p23 = assemble(&p01p12, &pxxp23, 1, 1);

            // p012 = lerp(p01, p12, t), p123 = lerp(p12, p23, t)
            let p012p123 = Sse41::add_ps(p01p12, Sse41::mul_ps(tttt,
                                                               Sse41::sub_ps(p12p23, p01p12)));

            let p123 = pluck(&p012p123, 1);

            // p0123 = lerp(p012, p123, t)
            let p0123 = Sse41::add_ps(p012p123,
                                      Sse41::mul_ps(tttt, Sse41::sub_ps(p123, p012p123)));

            let baseline0 = assemble(&p0p3, &p0123, 0, 0);
            let ctrl0 = assemble(&p01p12, &p012p123, 0, 0);
            let baseline1 = assemble(&p0123, &p0p3, 0, 1);
            let ctrl1 = assemble(&p012p123, &p12p23, 1, 1);

            // FIXME(pcwalton): Set flags appropriately!
            return (Segment {
                baseline: LineSegmentF32(baseline0),
                ctrl: LineSegmentF32(ctrl0),
                kind: SegmentKind::Cubic,
                flags: self.0.flags & SegmentFlags::FIRST_IN_SUBPATH,
            }, Segment {
                baseline: LineSegmentF32(baseline1),
                ctrl: LineSegmentF32(ctrl1),
                kind: SegmentKind::Cubic,
                flags: self.0.flags & SegmentFlags::CLOSES_SUBPATH,
            })
        }

        // Constructs a new 4-element vector from two pairs of adjacent lanes in two input vectors.
        unsafe fn assemble(a_data: &<Sse41 as Simd>::Vf32,
                           b_data: &<Sse41 as Simd>::Vf32,
                           a_index: usize,
                           b_index: usize)
                           -> <Sse41 as Simd>::Vf32 {
            let (a_data, b_data) = (Sse41::castps_pd(*a_data), Sse41::castps_pd(*b_data));
            let mut result = Sse41::setzero_pd();
            result[0] = a_data[a_index];
            result[1] = b_data[b_index];
            Sse41::castpd_ps(result)
        }

        // Constructs a new 2-element vector from a pair of adjacent lanes in an input vector.
        unsafe fn pluck(data: &<Sse41 as Simd>::Vf32, index: usize) -> <Sse41 as Simd>::Vf32 {
            let data = Sse41::castps_pd(*data);
            let mut result = Sse41::setzero_pd();
            result[0] = data[index];
            Sse41::castpd_ps(result)
        }
    }

    fn split_after(self, t: f32) -> Segment {
        self.split(t).1
    }

    fn y_extrema(self) -> (Option<f32>, Option<f32>) {
        let (t0, t1);
        unsafe {
            let mut p0p1p2p3 = Sse41::setzero_ps();
            p0p1p2p3[0] = self.0.baseline.from_y();
            p0p1p2p3[1] = self.0.ctrl.from_y();
            p0p1p2p3[2] = self.0.ctrl.to_y();
            p0p1p2p3[3] = self.0.baseline.to_y();

            let pxp0p1p2 = Sse41::shuffle_ps(p0p1p2p3, p0p1p2p3, 0b1001_0000);
            let pxv0v1v2 = Sse41::sub_ps(p0p1p2p3, pxp0p1p2);
            let (v0, v1, v2) = (pxv0v1v2[1], pxv0v1v2[2], pxv0v1v2[3]);

            let (v0_to_v1, v2_to_v1) = (v0 - v1, v2 - v1);
            let discrim = f32::sqrt(v1 * v1 - v0 * v2);
            let denom = 1.0 / (v0_to_v1 + v2_to_v1);

            t0 = (v0_to_v1 + discrim) * denom;
            t1 = (v0_to_v1 - discrim) * denom;
        }

        return match (t0 > EPSILON && t0 < 1.0 - EPSILON, t1 > EPSILON && t1 < 1.0 - EPSILON) {
            (false, false) => (None, None),
            (true, false) => (Some(t0), None),
            (false, true) => (Some(t1), None),
            (true, true) => (Some(f32::min(t0, t1)), Some(f32::max(t0, t1))),
        };

        const EPSILON: f32 = 0.001;
    }
}

// Tiling

const TILE_WIDTH: u32 = 16;
const TILE_HEIGHT: u32 = 16;

struct Tiler<'o, 'z> {
    outline: &'o Outline,
    built_object: BuiltObject,
    object_index: u16,
    z_buffer: &'z ZBuffer,

    point_queue: SortedVector<QueuedEndpoint>,
    active_edges: SortedVector<ActiveEdge>,
    old_active_edges: Vec<ActiveEdge>,
}

impl<'o, 'z> Tiler<'o, 'z> {
    #[allow(clippy::or_fun_call)]
    fn new(outline: &'o Outline,
           view_box: &Rect<f32>,
           object_index: u16,
           shader: ShaderId,
           z_buffer: &'z ZBuffer)
           -> Tiler<'o, 'z> {
        let bounds = outline.bounds.intersection(&view_box).unwrap_or(Rect::zero());
        let built_object = BuiltObject::new(&bounds, shader);

        Tiler {
            outline,
            built_object,
            object_index,
            z_buffer,

            point_queue: SortedVector::new(),
            active_edges: SortedVector::new(),
            old_active_edges: vec![],
        }
    }

    fn generate_tiles(&mut self) {
        // Initialize the point queue.
        self.init_point_queue();

        // Reset active edges.
        self.active_edges.clear();
        self.old_active_edges.clear();

        // Generate strips.
        let tile_rect = self.built_object.tile_rect;
        for strip_origin_y in tile_rect.origin.y..tile_rect.max_y() {
            self.generate_strip(strip_origin_y);
        }

        // Cull.
        self.cull();
        //println!("{:#?}", self.built_object);
    }

    fn generate_strip(&mut self, strip_origin_y: i16) {
        // Process old active edges.
        self.process_old_active_edges(strip_origin_y);

        // Add new active edges.
        let strip_max_y = ((i32::from(strip_origin_y) + 1) * TILE_HEIGHT as i32) as f32;
        while let Some(queued_endpoint) = self.point_queue.peek() {
            if queued_endpoint.y >= strip_max_y {
                break
            }
            self.add_new_active_edge(strip_origin_y);
        }
    }

    fn cull(&self) {
        for solid_tile_index in self.built_object.solid_tiles.ones() {
            let tile = &self.built_object.tiles[solid_tile_index];
            if tile.backdrop != 0 {
                self.z_buffer.update(tile.tile_x, tile.tile_y, self.object_index);
            }
        }
    }

    fn process_old_active_edges(&mut self, tile_y: i16) {
        let mut current_tile_x = self.built_object.tile_rect.origin.x;
        let mut current_subtile_x = 0.0;
        let mut current_winding = 0;

        debug_assert!(self.old_active_edges.is_empty());
        mem::swap(&mut self.old_active_edges, &mut self.active_edges.array);

        let mut last_segment_x = -9999.0;

        let tile_top = (i32::from(tile_y) * TILE_HEIGHT as i32) as f32;
        //println!("---------- tile y {}({}) ----------", tile_y, tile_top);
        //println!("old active edges: {:#?}", self.old_active_edges);

        for mut active_edge in self.old_active_edges.drain(..) {
            // Determine x-intercept and winding.
            let segment_x = active_edge.crossing.x();
            let edge_winding =
                if active_edge.segment.baseline.from_y() < active_edge.segment.baseline.to_y() {
                    1
                } else {
                    -1
                };

            /*
            println!("tile Y {}({}): segment_x={} edge_winding={} current_tile_x={} \
                      current_subtile_x={} current_winding={}",
                     tile_y,
                     tile_top,
                     segment_x,
                     edge_winding,
                     current_tile_x,
                     current_subtile_x,
                     current_winding);
            println!("... segment={:#?} crossing={:?}", active_edge.segment, active_edge.crossing);
            */

            // FIXME(pcwalton): Remove this debug code!
            debug_assert!(segment_x >= last_segment_x);
            last_segment_x = segment_x;

            // Do initial subtile fill, if necessary.
            let segment_tile_x = (f32::floor(segment_x) as i32 / TILE_WIDTH as i32) as i16;
            if current_tile_x < segment_tile_x && current_subtile_x > 0.0 {
                let current_x = (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32 +
                    current_subtile_x;
                let tile_right_x = ((i32::from(current_tile_x) + 1) * TILE_WIDTH as i32) as f32;
                self.built_object.add_active_fill(current_x,
                                                  tile_right_x,
                                                  current_winding,
                                                  current_tile_x,
                                                  tile_y);
                current_tile_x += 1;
                current_subtile_x = 0.0;
            }

            // Move over to the correct tile, filling in as we go.
            while current_tile_x < segment_tile_x {
                //println!("... emitting backdrop {} @ tile {}", current_winding, current_tile_x);
                self.built_object.get_tile_mut(current_tile_x, tile_y).backdrop = current_winding;
                current_tile_x += 1;
                current_subtile_x = 0.0;
            }

            // Do final subtile fill, if necessary.
            debug_assert!(current_tile_x == segment_tile_x);
            debug_assert!(current_tile_x < self.built_object.tile_rect.max_x());
            let segment_subtile_x =
                segment_x - (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32;
            if segment_subtile_x > current_subtile_x {
                let current_x = (i32::from(current_tile_x) * TILE_WIDTH as i32) as f32 +
                    current_subtile_x;
                self.built_object.add_active_fill(current_x,
                                                  segment_x,
                                                  current_winding,
                                                  current_tile_x,
                                                  tile_y);
                current_subtile_x = segment_subtile_x;
            }

            // Update winding.
            current_winding += edge_winding;

            // Process the edge.
            //println!("about to process existing active edge {:#?}", active_edge);
            debug_assert!(f32::abs(active_edge.crossing.y() - tile_top) < 0.1);
            active_edge.process(&mut self.built_object, tile_y);
            if !active_edge.segment.is_none() {
                self.active_edges.push(active_edge);
            }
        }

        //debug_assert_eq!(current_winding, 0);
    }

    fn add_new_active_edge(&mut self, tile_y: i16) {
        let outline = &self.outline;
        let point_index = self.point_queue.pop().unwrap().point_index;

        let contour = &outline.contours[point_index.contour() as usize];

        // TODO(pcwalton): Could use a bitset of processed edges…
        let prev_endpoint_index = contour.prev_endpoint_index_of(point_index.point());
        let next_endpoint_index = contour.next_endpoint_index_of(point_index.point());
        /*
        println!("adding new active edge, tile_y={} point_index={} prev={} next={} pos={:?} \
                  prevpos={:?} nextpos={:?}",
                 tile_y,
                 point_index.point(),
                 prev_endpoint_index,
                 next_endpoint_index,
                 contour.position_of(point_index.point()),
                 contour.position_of(prev_endpoint_index),
                 contour.position_of(next_endpoint_index));
        */

        if contour.point_is_logically_above(point_index.point(), prev_endpoint_index) {
            //println!("... adding prev endpoint");
            process_active_segment(contour,
                                   prev_endpoint_index,
                                   &mut self.active_edges,
                                   &mut self.built_object,
                                   tile_y);

            self.point_queue.push(QueuedEndpoint {
                point_index: PointIndex::new(point_index.contour(), prev_endpoint_index),
                y: contour.position_of(prev_endpoint_index).y(),
            });
            //println!("... done adding prev endpoint");
        }

        if contour.point_is_logically_above(point_index.point(), next_endpoint_index) {
            /*
            println!("... adding next endpoint {} -> {}",
                     point_index.point(),
                     next_endpoint_index);
            */
            process_active_segment(contour,
                                   point_index.point(),
                                   &mut self.active_edges,
                                   &mut self.built_object,
                                   tile_y);

            self.point_queue.push(QueuedEndpoint {
                point_index: PointIndex::new(point_index.contour(), next_endpoint_index),
                y: contour.position_of(next_endpoint_index).y(),
            });
            //println!("... done adding next endpoint");
        }
    }

    fn init_point_queue(&mut self) {
        // Find MIN points.
        self.point_queue.clear();
        for (contour_index, contour) in self.outline.contours.iter().enumerate() {
            let contour_index = contour_index as u32;
            let mut cur_endpoint_index = 0;
            let mut prev_endpoint_index = contour.prev_endpoint_index_of(cur_endpoint_index);
            let mut next_endpoint_index = contour.next_endpoint_index_of(cur_endpoint_index);
            loop {
                if contour.point_is_logically_above(cur_endpoint_index, prev_endpoint_index) &&
                        contour.point_is_logically_above(cur_endpoint_index, next_endpoint_index) {
                    self.point_queue.push(QueuedEndpoint {
                        point_index: PointIndex::new(contour_index, cur_endpoint_index),
                        y: contour.position_of(cur_endpoint_index).y(),
                    });
                }

                if cur_endpoint_index >= next_endpoint_index {
                    break
                }

                prev_endpoint_index = cur_endpoint_index;
                cur_endpoint_index = next_endpoint_index;
                next_endpoint_index = contour.next_endpoint_index_of(cur_endpoint_index);
            }
        }
    }
}

fn process_active_segment(contour: &Contour,
                          from_endpoint_index: u32,
                          active_edges: &mut SortedVector<ActiveEdge>,
                          built_object: &mut BuiltObject,
                          tile_y: i16) {
    let mut active_edge = ActiveEdge::from_segment(&contour.segment_after(from_endpoint_index));
    //println!("... process_active_segment({:#?})", active_edge);
    active_edge.process(built_object, tile_y);
    if !active_edge.segment.is_none() {
        active_edges.push(active_edge);
    }
}

// Scene construction

impl BuiltScene {
    fn new(view_box: &Rect<f32>) -> BuiltScene {
        BuiltScene { view_box: *view_box, batches: vec![], solid_tiles: vec![], shaders: vec![] }
    }
}

fn scene_tile_index(tile_x: i16, tile_y: i16, tile_rect: Rect<i16>) -> u32 {
    (tile_y - tile_rect.origin.y) as u32 * tile_rect.size.width as u32 +
        (tile_x - tile_rect.origin.x) as u32
}

struct SceneBuilder {
    objects: Vec<BuiltObject>,
    z_buffer: ZBuffer,
    tile_rect: Rect<i16>,

    current_object_index: usize,
}

impl SceneBuilder {
    fn new(objects: Vec<BuiltObject>, z_buffer: ZBuffer, view_box: &Rect<f32>) -> SceneBuilder {
        let tile_rect = round_rect_out_to_tile_bounds(view_box);
        SceneBuilder { objects, z_buffer, tile_rect, current_object_index: 0 }
    }

    fn build_solid_tiles(&self) -> Vec<SolidTileScenePrimitive> {
        self.z_buffer.build_solid_tiles(&self.objects, &self.tile_rect)
    }

    fn build_batch(&mut self) -> Option<Batch> {
        let mut batch = Batch::new();

        let mut object_tile_index_to_batch_mask_tile_index = vec![];
        while self.current_object_index < self.objects.len() {
            let object = &self.objects[self.current_object_index];

            if batch.fills.len() + object.fills.len() > MAX_FILLS_PER_BATCH {
                break;
            }

            object_tile_index_to_batch_mask_tile_index.clear();
            object_tile_index_to_batch_mask_tile_index.extend(
                iter::repeat(u16::MAX).take(object.tiles.len()));

            // Copy mask tiles.
            for (tile_index, tile) in object.tiles.iter().enumerate() {
                // Skip solid tiles, since we handled them above already.
                if object.solid_tiles[tile_index] {
                    continue;
                }

                // Cull occluded tiles.
                let scene_tile_index = scene_tile_index(tile.tile_x, tile.tile_y, self.tile_rect);
                if !self.z_buffer.test(scene_tile_index, self.current_object_index as u32) {
                    continue;
                }

                // Visible mask tile.
                let batch_mask_tile_index = batch.mask_tiles.len() as u16;
                if batch_mask_tile_index == MAX_MASKS_PER_BATCH {
                    break;
                }

                object_tile_index_to_batch_mask_tile_index[tile_index] = batch_mask_tile_index;

                batch.mask_tiles.push(MaskTileBatchPrimitive {
                    tile: *tile,
                    shader: object.shader,
                });
            }

            // Remap and copy fills, culling as necessary.
            for fill in &object.fills {
                let object_tile_index = object.tile_coords_to_index(fill.tile_x, fill.tile_y);
                let mask_tile_index =
                    object_tile_index_to_batch_mask_tile_index[object_tile_index as usize];
                if mask_tile_index < u16::MAX {
                    batch.fills.push(FillBatchPrimitive {
                        px: fill.px,
                        subpx: fill.subpx,
                        mask_tile_index,
                    });
                }
            }

            self.current_object_index += 1;
        }

        if batch.is_empty() {
            None
        } else {
            Some(batch)
        }
    }
}

// Culling

struct ZBuffer {
    buffer: Vec<AtomicUsize>,
    tile_rect: Rect<i16>,
}

impl ZBuffer {
    fn new(view_box: &Rect<f32>) -> ZBuffer {
        let tile_rect = round_rect_out_to_tile_bounds(view_box);
        let tile_area = tile_rect.size.width as usize * tile_rect.size.height as usize;
        ZBuffer {
            buffer: (0..tile_area).map(|_| AtomicUsize::new(0)).collect(),
            tile_rect,
        }
    }

    fn test(&self, scene_tile_index: u32, object_index: u32) -> bool {
        let existing_depth = self.buffer[scene_tile_index as usize].load(AtomicOrdering::SeqCst);
        existing_depth < object_index as usize + 1
    }

    fn update(&self, tile_x: i16, tile_y: i16, object_index: u16) {
        let scene_tile_index = scene_tile_index(tile_x, tile_y, self.tile_rect) as usize;
        let mut old_depth = self.buffer[scene_tile_index].load(AtomicOrdering::SeqCst);
        let new_depth = (object_index + 1) as usize;
        while old_depth < new_depth {
            let prev_depth = self.buffer[scene_tile_index]
                                 .compare_and_swap(old_depth,
                                                   new_depth,
                                                   AtomicOrdering::SeqCst);
            if prev_depth == old_depth {
                // Successfully written.
                return
            }
            old_depth = prev_depth;
        }
    }

    fn build_solid_tiles(&self, objects: &[BuiltObject], tile_rect: &Rect<i16>)
                         -> Vec<SolidTileScenePrimitive> {
        let mut solid_tiles = vec![];
        for scene_tile_y in 0..tile_rect.size.height {
            for scene_tile_x in 0..tile_rect.size.width {
                let scene_tile_index = scene_tile_y as usize * tile_rect.size.width as usize +
                    scene_tile_x as usize;
                let depth = self.buffer[scene_tile_index].load(AtomicOrdering::Relaxed);
                if depth == 0 {
                    continue
                }
                let object_index = (depth - 1) as usize;
                solid_tiles.push(SolidTileScenePrimitive {
                    tile_x: scene_tile_x + tile_rect.origin.x,
                    tile_y: scene_tile_y + tile_rect.origin.y,
                    shader: objects[object_index].shader,
                });
            }
        }

        solid_tiles
    }
}

// Primitives

#[derive(Debug)]
struct BuiltObject {
    bounds: Rect<f32>,
    tile_rect: Rect<i16>,
    tiles: Vec<TileObjectPrimitive>,
    fills: Vec<FillObjectPrimitive>,
    solid_tiles: FixedBitSet,
    shader: ShaderId,
}

#[derive(Debug)]
struct BuiltScene {
    view_box: Rect<f32>,
    batches: Vec<Batch>,
    solid_tiles: Vec<SolidTileScenePrimitive>,
    shaders: Vec<ObjectShader>,
}

#[derive(Debug)]
struct Batch {
    fills: Vec<FillBatchPrimitive>,
    mask_tiles: Vec<MaskTileBatchPrimitive>,
}

#[derive(Clone, Copy, Debug)]
struct FillObjectPrimitive {
    px: LineSegmentU4,
    subpx: LineSegmentU8,
    tile_x: i16,
    tile_y: i16,
}

#[derive(Clone, Copy, Debug)]
struct TileObjectPrimitive {
    tile_x: i16,
    tile_y: i16,
    backdrop: i16,
}

#[derive(Clone, Copy, Debug)]
struct FillBatchPrimitive {
    px: LineSegmentU4,
    subpx: LineSegmentU8,
    mask_tile_index: u16,
}

#[derive(Clone, Copy, Debug)]
struct SolidTileScenePrimitive {
    tile_x: i16,
    tile_y: i16,
    shader: ShaderId,
}

#[derive(Clone, Copy, Debug)]
struct MaskTileBatchPrimitive {
    tile: TileObjectPrimitive,
    shader: ShaderId,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ShaderId(pub u16);

#[derive(Clone, Copy, Debug, Default)]
struct ObjectShader {
    fill_color: ColorU,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
struct ColorU {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

// Utilities for built objects

impl BuiltObject {
    fn new(bounds: &Rect<f32>, shader: ShaderId) -> BuiltObject {
        // Compute the tile rect.
        let tile_rect = round_rect_out_to_tile_bounds(&bounds);

        // Allocate tiles.
        let tile_count = tile_rect.size.width as usize * tile_rect.size.height as usize;
        let mut tiles = Vec::with_capacity(tile_count);
        for y in tile_rect.origin.y..tile_rect.max_y() {
            for x in tile_rect.origin.x..tile_rect.max_x() {
                tiles.push(TileObjectPrimitive::new(x, y));
            }
        }

        let mut solid_tiles = FixedBitSet::with_capacity(tile_count);
        solid_tiles.insert_range(..);

        BuiltObject {
            bounds: *bounds,
            tile_rect,
            tiles,
            fills: vec![],
            solid_tiles,
            shader,
        }
    }

    // TODO(pcwalton): SIMD-ify `tile_x` and `tile_y`.
    fn add_fill(&mut self, segment: &LineSegmentF32, tile_x: i16, tile_y: i16) {
        //println!("add_fill({:?} ({}, {}))", segment, tile_x, tile_y);
        let (px, subpx);
        unsafe {
            let mut segment = Sse41::cvtps_epi32(Sse41::mul_ps(segment.0, Sse41::set1_ps(256.0)));

            let mut tile_origin = Sse41::setzero_epi32();
            tile_origin[0] = (tile_x as i32) * (TILE_WIDTH as i32) * 256;
            tile_origin[1] = (tile_y as i32) * (TILE_HEIGHT as i32) * 256;
            tile_origin = Sse41::shuffle_epi32(tile_origin, 0b0100_0100);

            segment = Sse41::sub_epi32(segment, tile_origin);
            /*
            println!("... before min: {} {} {} {}",
                     segment[0], segment[1], segment[2], segment[3]);
            */
            //segment = Sse41::max_epi32(segment, Sse41::setzero_epi32());
            segment = Sse41::min_epi32(segment, Sse41::set1_epi32(0x0fff));
            //println!("... after min: {} {} {} {}", segment[0], segment[1], segment[2], segment[3]);

            let mut shuffle_mask = Sse41::setzero_epi32();
            shuffle_mask[0] = 0x0c08_0400;
            shuffle_mask[1] = 0x0d05_0901;
            segment = Sse41::shuffle_epi8(segment, shuffle_mask);

            px = LineSegmentU4((segment[1] | (segment[1] >> 12)) as u16);
            subpx = LineSegmentU8(segment[0] as u32);
        }

        let tile_index = self.tile_coords_to_index(tile_x, tile_y);

        /*
        // TODO(pcwalton): Cull degenerate fills again.
        // Cull degenerate fills.
        let (from_px, to_px) = (from.to_u8(), to.to_u8());
        if from_px.x == to_px.x && from_subpx.x == to_subpx.x {
            return
        }
        */

        self.fills.push(FillObjectPrimitive { px, subpx, tile_x, tile_y });
        self.solid_tiles.set(tile_index as usize, false);
    }

    fn add_active_fill(&mut self,
                       left: f32,
                       right: f32,
                       mut winding: i16,
                       tile_x: i16,
                       tile_y: i16) {
        let tile_origin_y = (i32::from(tile_y) * TILE_HEIGHT as i32) as f32;
        let left = Point2DF32::new(left, tile_origin_y);
        let right = Point2DF32::new(right, tile_origin_y);

        let segment = if winding < 0 {
            LineSegmentF32::new(&left, &right)
        } else {
            LineSegmentF32::new(&right, &left)
        };

        /*
        println!("... emitting active fill {} -> {} winding {} @ tile {}",
                 left.x(),
                 right.x(),
                 winding,
                 tile_x);
        */

        while winding != 0 {
            self.add_fill(&segment, tile_x, tile_y);
            if winding < 0 {
                winding += 1
            } else {
                winding -= 1
            }
        }
    }

    // TODO(pcwalton): Optimize this better with SIMD!
    fn generate_fill_primitives_for_line(&mut self, mut segment: LineSegmentF32, tile_y: i16) {
        /*
        println!("... generate_fill_primitives_for_line(): segment={:?} tile_y={} ({}-{})",
                    segment,
                    tile_y,
                    tile_y as f32 * TILE_HEIGHT as f32,
                    (tile_y + 1) as f32 * TILE_HEIGHT as f32);
        */

        let winding = segment.from_x() > segment.to_x();
        let (segment_left, segment_right) = if !winding {
            (segment.from_x(), segment.to_x())
        } else {
            (segment.to_x(), segment.from_x())
        };

        let segment_tile_left = (f32::floor(segment_left) as i32 / TILE_WIDTH as i32) as i16;
        let segment_tile_right = util::alignup_i32(f32::ceil(segment_right) as i32,
                                                   TILE_WIDTH as i32) as i16;

        for subsegment_tile_x in segment_tile_left..segment_tile_right {
            let (mut fill_from, mut fill_to) = (segment.from(), segment.to());
            let subsegment_tile_right =
                ((i32::from(subsegment_tile_x) + 1) * TILE_HEIGHT as i32) as f32;
            if subsegment_tile_right < segment_right {
                let x = subsegment_tile_right;
                let point = Point2DF32::new(x, segment.solve_y_for_x(x));
                if !winding {
                    fill_to = point;
                    segment = LineSegmentF32::new(&point, &segment.to());
                } else {
                    fill_from = point;
                    segment = LineSegmentF32::new(&segment.from(), &point);
                }
            }

            let fill_segment = LineSegmentF32::new(&fill_from, &fill_to);
            self.add_fill(&fill_segment, subsegment_tile_x, tile_y);
        }
    }

    // FIXME(pcwalton): Use a `Point2D<i16>` instead?
    fn tile_coords_to_index(&self, tile_x: i16, tile_y: i16) -> u32 {
        /*println!("tile_coords_to_index(x={}, y={}, tile_rect={:?})",
                 tile_x,
                 tile_y,
                 self.tile_rect);*/
        (tile_y - self.tile_rect.origin.y) as u32 * self.tile_rect.size.width as u32 +
            (tile_x - self.tile_rect.origin.x) as u32
    }

    fn get_tile_mut(&mut self, tile_x: i16, tile_y: i16) -> &mut TileObjectPrimitive {
        let tile_index = self.tile_coords_to_index(tile_x, tile_y);
        &mut self.tiles[tile_index as usize]
    }
}

impl Paint {
    fn from_svg_paint(svg_paint: &UsvgPaint) -> Paint {
        Paint {
            color: match *svg_paint {
                UsvgPaint::Color(color) => ColorU::from_svg_color(color),
                UsvgPaint::Link(_) => {
                    // TODO(pcwalton)
                    ColorU::black()
                }
            },
        }
    }
}

// Scene serialization

impl BuiltScene {
    fn write<W>(&self, writer: &mut W) -> io::Result<()> where W: Write {
        writer.write_all(b"RIFF")?;

        let header_size = 4 * 6;

        let solid_tiles_size = self.solid_tiles.len() * mem::size_of::<SolidTileScenePrimitive>();

        let batch_sizes: Vec<_> = self.batches.iter().map(|batch| {
            BatchSizes {
                fills: (batch.fills.len() * mem::size_of::<FillBatchPrimitive>()),
                mask_tiles: (batch.mask_tiles.len() * mem::size_of::<MaskTileBatchPrimitive>()),
            }
        }).collect();

        let total_batch_sizes: usize = batch_sizes.iter().map(|sizes| 8 + sizes.total()).sum();

        let shaders_size = self.shaders.len() * mem::size_of::<ObjectShader>();

        writer.write_u32::<LittleEndian>((4 +
                                          8 + header_size +
                                          8 + solid_tiles_size +
                                          8 + shaders_size +
                                          total_batch_sizes) as u32)?;

        writer.write_all(b"PF3S")?;

        writer.write_all(b"head")?;
        writer.write_u32::<LittleEndian>(header_size as u32)?;
        writer.write_u32::<LittleEndian>(FILE_VERSION)?;
        writer.write_u32::<LittleEndian>(self.batches.len() as u32)?;
        writer.write_f32::<LittleEndian>(self.view_box.origin.x)?;
        writer.write_f32::<LittleEndian>(self.view_box.origin.y)?;
        writer.write_f32::<LittleEndian>(self.view_box.size.width)?;
        writer.write_f32::<LittleEndian>(self.view_box.size.height)?;

        writer.write_all(b"shad")?;
        writer.write_u32::<LittleEndian>(shaders_size as u32)?;
        for &shader in &self.shaders {
            let fill_color = shader.fill_color;
            writer.write_all(&[fill_color.r, fill_color.g, fill_color.b, fill_color.a])?;
        }

        writer.write_all(b"soli")?;
        writer.write_u32::<LittleEndian>(solid_tiles_size as u32)?;
        for &tile_primitive in &self.solid_tiles {
            writer.write_i16::<LittleEndian>(tile_primitive.tile_x)?;
            writer.write_i16::<LittleEndian>(tile_primitive.tile_y)?;
            writer.write_u16::<LittleEndian>(tile_primitive.shader.0)?;
        }

        for (batch, sizes) in self.batches.iter().zip(batch_sizes.iter()) {
            writer.write_all(b"batc")?;
            writer.write_u32::<LittleEndian>(sizes.total() as u32)?;

            writer.write_all(b"fill")?;
            writer.write_u32::<LittleEndian>(sizes.fills as u32)?;
            for fill_primitive in &batch.fills {
                writer.write_u16::<LittleEndian>(fill_primitive.px.0)?;
                writer.write_u32::<LittleEndian>(fill_primitive.subpx.0)?;
                writer.write_u16::<LittleEndian>(fill_primitive.mask_tile_index)?;
            }

            writer.write_all(b"mask")?;
            writer.write_u32::<LittleEndian>(sizes.mask_tiles as u32)?;
            for &tile_primitive in &batch.mask_tiles {
                writer.write_i16::<LittleEndian>(tile_primitive.tile.tile_x)?;
                writer.write_i16::<LittleEndian>(tile_primitive.tile.tile_y)?;
                writer.write_i16::<LittleEndian>(tile_primitive.tile.backdrop)?;
                writer.write_u16::<LittleEndian>(tile_primitive.shader.0)?;
            }
        }

        return Ok(());

        const FILE_VERSION: u32 = 0;

        struct BatchSizes {
            fills: usize,
            mask_tiles: usize,
        }

        impl BatchSizes {
            fn total(&self) -> usize {
                8 + self.fills + 8 + self.mask_tiles
            }
        }
    }
}

impl Batch {
    fn new() -> Batch {
        Batch { fills: vec![], mask_tiles: vec![] }
    }

    fn is_empty(&self) -> bool { self.mask_tiles.is_empty() }
}

impl TileObjectPrimitive {
    fn new(tile_x: i16, tile_y: i16) -> TileObjectPrimitive {
        TileObjectPrimitive { tile_x, tile_y, backdrop: 0 }
    }
}

impl ColorU {
    fn black() -> ColorU {
        ColorU { r: 0, g: 0, b: 0, a: 255 }
    }

    fn from_svg_color(svg_color: SvgColor) -> ColorU {
        ColorU { r: svg_color.red, g: svg_color.green, b: svg_color.blue, a: 255 }
    }
}

// Tile geometry utilities

fn round_rect_out_to_tile_bounds(rect: &Rect<f32>) -> Rect<i16> {
    let tile_origin = Point2D::new((f32::floor(rect.origin.x) as i32 / TILE_WIDTH as i32) as i16,
                                   (f32::floor(rect.origin.y) as i32 / TILE_HEIGHT as i32) as i16);
    let tile_extent =
        Point2D::new(util::alignup_i32(f32::ceil(rect.max_x()) as i32, TILE_WIDTH as i32) as i16,
                     util::alignup_i32(f32::ceil(rect.max_y()) as i32, TILE_HEIGHT as i32) as i16);
    let tile_size = Size2D::new(tile_extent.x - tile_origin.x, tile_extent.y - tile_origin.y);
    Rect::new(tile_origin, tile_size)
}

// USVG stuff

fn usvg_rect_to_euclid_rect(rect: &UsvgRect) -> Rect<f32> {
    Rect::new(Point2D::new(rect.x, rect.y), Size2D::new(rect.width, rect.height)).to_f32()
}

fn usvg_transform_to_transform_2d(transform: &UsvgTransform) -> Transform2DF32 {
    Transform2DF32::row_major(transform.a as f32, transform.b as f32,
                              transform.c as f32, transform.d as f32,
                              transform.e as f32, transform.f as f32)
}

struct UsvgPathToSegments<I> where I: Iterator<Item = UsvgPathSegment> {
    iter: I,
    first_subpath_point: Point2DF32,
    last_subpath_point: Point2DF32,
    just_moved: bool,
}

impl<I> UsvgPathToSegments<I> where I: Iterator<Item = UsvgPathSegment> {
    fn new(iter: I) -> UsvgPathToSegments<I> {
        UsvgPathToSegments {
            iter,
            first_subpath_point: Point2DF32::default(),
            last_subpath_point: Point2DF32::default(),
            just_moved: false,
        }
    }
}

impl<I> Iterator for UsvgPathToSegments<I> where I: Iterator<Item = UsvgPathSegment> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        match self.iter.next()? {
            UsvgPathSegment::MoveTo { x, y } => {
                let to = Point2DF32::new(x as f32, y as f32);
                self.first_subpath_point = to;
                self.last_subpath_point = to;
                self.just_moved = true;
                self.next()
            }
            UsvgPathSegment::LineTo { x, y } => {
                let to = Point2DF32::new(x as f32, y as f32);
                let mut segment =
                    Segment::line(&LineSegmentF32::new(&self.last_subpath_point, &to));
                if self.just_moved {
                    segment.flags.insert(SegmentFlags::FIRST_IN_SUBPATH);
                }
                self.last_subpath_point = to;
                self.just_moved = false;
                Some(segment)
            }
            UsvgPathSegment::CurveTo { x1, y1, x2, y2, x, y } => {
                let ctrl0 = Point2DF32::new(x1 as f32, y1 as f32);
                let ctrl1 = Point2DF32::new(x2 as f32, y2 as f32);
                let to = Point2DF32::new(x as f32, y as f32);
                let mut segment =
                    Segment::cubic(&LineSegmentF32::new(&self.last_subpath_point, &to),
                                   &LineSegmentF32::new(&ctrl0, &ctrl1));
                if self.just_moved {
                    segment.flags.insert(SegmentFlags::FIRST_IN_SUBPATH);
                }
                self.last_subpath_point = to;
                self.just_moved = false;
                Some(segment)
            }
            UsvgPathSegment::ClosePath => {
                let mut segment = Segment::line(&LineSegmentF32::new(&self.last_subpath_point,
                                                                     &self.first_subpath_point));
                segment.flags.insert(SegmentFlags::CLOSES_SUBPATH);
                self.just_moved = false;
                self.last_subpath_point = self.first_subpath_point;
                Some(segment)
            }
        }
    }
}

// Euclid interoperability
//
// TODO(pcwalton): Remove this once we're fully on Pathfinder's native geometry.

struct PathEventsToSegments<I> where I: Iterator<Item = PathEvent> {
    iter: I,
    first_subpath_point: Point2DF32,
    last_subpath_point: Point2DF32,
    just_moved: bool,
}

impl<I> PathEventsToSegments<I> where I: Iterator<Item = PathEvent> {
    fn new(iter: I) -> PathEventsToSegments<I> {
        PathEventsToSegments {
            iter,
            first_subpath_point: Point2DF32::default(),
            last_subpath_point: Point2DF32::default(),
            just_moved: false,
        }
    }
}

impl<I> Iterator for PathEventsToSegments<I> where I: Iterator<Item = PathEvent> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        match self.iter.next()? {
            PathEvent::MoveTo(to) => {
                let to = Point2DF32::from_euclid(to);
                self.first_subpath_point = to;
                self.last_subpath_point = to;
                self.just_moved = true;
                self.next()
            }
            PathEvent::LineTo(to) => {
                let to = Point2DF32::from_euclid(to);
                let mut segment =
                    Segment::line(&LineSegmentF32::new(&self.last_subpath_point, &to));
                if self.just_moved {
                    segment.flags.insert(SegmentFlags::FIRST_IN_SUBPATH);
                }
                self.last_subpath_point = to;
                self.just_moved = false;
                Some(segment)
            }
            PathEvent::QuadraticTo(ctrl, to) => {
                let (ctrl, to) = (Point2DF32::from_euclid(ctrl), Point2DF32::from_euclid(to));
                let mut segment =
                    Segment::quadratic(&LineSegmentF32::new(&self.last_subpath_point, &to),
                                       &ctrl);
                if self.just_moved {
                    segment.flags.insert(SegmentFlags::FIRST_IN_SUBPATH);
                }
                self.last_subpath_point = to;
                self.just_moved = false;
                Some(segment)
            }
            PathEvent::CubicTo(ctrl0, ctrl1, to) => {
                let ctrl0 = Point2DF32::from_euclid(ctrl0);
                let ctrl1 = Point2DF32::from_euclid(ctrl1);
                let to = Point2DF32::from_euclid(to);
                let mut segment =
                    Segment::cubic(&LineSegmentF32::new(&self.last_subpath_point, &to),
                                   &LineSegmentF32::new(&ctrl0, &ctrl1));
                if self.just_moved {
                    segment.flags.insert(SegmentFlags::FIRST_IN_SUBPATH);
                }
                self.last_subpath_point = to;
                self.just_moved = false;
                Some(segment)
            }
            PathEvent::Close => {
                let mut segment = Segment::line(&LineSegmentF32::new(&self.last_subpath_point,
                                                                     &self.first_subpath_point));
                segment.flags.insert(SegmentFlags::CLOSES_SUBPATH);
                self.just_moved = false;
                self.last_subpath_point = self.first_subpath_point;
                Some(segment)
            }
            PathEvent::Arc(..) => panic!("TODO: arcs"),
        }
    }
}

struct SegmentsToPathEvents<I> where I: Iterator<Item = Segment> {
    iter: I,
    buffer: Option<PathEvent>,
}

impl<I> SegmentsToPathEvents<I> where I: Iterator<Item = Segment> {
    fn new(iter: I) -> SegmentsToPathEvents<I> {
        SegmentsToPathEvents { iter, buffer: None }
    }
}

impl<I> Iterator for SegmentsToPathEvents<I> where I: Iterator<Item = Segment> {
    type Item = PathEvent;

    fn next(&mut self) -> Option<PathEvent> {
        if let Some(event) = self.buffer.take() {
            return Some(event);
        }

        let segment = self.iter.next()?;
        if segment.flags.contains(SegmentFlags::CLOSES_SUBPATH) {
            return Some(PathEvent::Close);
        }

        let event = match segment.kind {
            SegmentKind::None => return self.next(),
            SegmentKind::Line => PathEvent::LineTo(segment.baseline.to().as_euclid()),
            SegmentKind::Quadratic => {
                PathEvent::QuadraticTo(segment.ctrl.from().as_euclid(),
                                       segment.baseline.to().as_euclid())
            }
            SegmentKind::Cubic => {
                PathEvent::CubicTo(segment.ctrl.from().as_euclid(),
                                   segment.ctrl.to().as_euclid(),
                                   segment.baseline.to().as_euclid())
            }
        };

        if segment.flags.contains(SegmentFlags::FIRST_IN_SUBPATH) {
            self.buffer = Some(event);
            Some(PathEvent::MoveTo(segment.baseline.from().as_euclid()))
        } else {
            Some(event)
        }
    }
}

// Path transformation utilities

struct PathTransformingIter<I> where I: Iterator<Item = Segment> {
    iter: I,
    transform: Transform2DF32,
}

impl<I> Iterator for PathTransformingIter<I> where I: Iterator<Item = Segment> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        // TODO(pcwalton): Can we go faster by transforming an entire line segment with SIMD?
        let mut segment = self.iter.next()?;
        if !segment.is_none() {
            segment.baseline.set_from(&self.transform.transform_point(&segment.baseline.from()));
            segment.baseline.set_to(&self.transform.transform_point(&segment.baseline.to()));
            if !segment.is_line() {
                segment.ctrl.set_from(&self.transform.transform_point(&segment.ctrl.from()));
                if !segment.is_quadratic() {
                    segment.ctrl.set_to(&self.transform.transform_point(&segment.ctrl.to()));
                }
            }
        }
        Some(segment)
    }
}

impl<I> PathTransformingIter<I> where I: Iterator<Item = Segment> {
    fn new(iter: I, transform: &Transform2DF32) -> PathTransformingIter<I> {
        PathTransformingIter { iter, transform: *transform }
    }
}

// Monotonic conversion utilities

// TODO(pcwalton): I think we only need to be monotonic in Y, maybe?
struct MonotonicConversionIter<I> where I: Iterator<Item = Segment> {
    iter: I,
    buffer: ArrayVec<[Segment; 2]>,
}

impl<I> Iterator for MonotonicConversionIter<I> where I: Iterator<Item = Segment> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        if let Some(segment) = self.buffer.pop() {
            return Some(segment);
        }

        let segment = self.iter.next()?;
        match segment.kind {
            SegmentKind::None => self.next(),
            SegmentKind::Line => Some(segment),
            SegmentKind::Cubic => self.handle_cubic(&segment),
            SegmentKind::Quadratic => {
                // TODO(pcwalton): Don't degree elevate!
                self.handle_cubic(&segment.to_cubic())
            }
        }
    }
}

impl<I> MonotonicConversionIter<I> where I: Iterator<Item = Segment> {
    fn new(iter: I) -> MonotonicConversionIter<I> {
        MonotonicConversionIter { iter, buffer: ArrayVec::new() }
    }

    fn handle_cubic(&mut self, segment: &Segment) -> Option<Segment> {
        match segment.as_cubic_segment().y_extrema() {
            (Some(t0), Some(t1)) => {
                let (segments_01, segment_2) = segment.as_cubic_segment().split(t1);
                self.buffer.push(segment_2);
                let (segment_0, segment_1) = segments_01.as_cubic_segment().split(t0 / t1);
                self.buffer.push(segment_1);
                Some(segment_0)
            }
            (Some(t0), None) | (None, Some(t0)) => {
                let (segment_0, segment_1) = segment.as_cubic_segment().split(t0);
                self.buffer.push(segment_1);
                Some(segment_0)
            }
            (None, None) => Some(*segment),
        }
    }
}

// SortedVector

#[derive(Clone, Debug)]
pub struct SortedVector<T> where T: PartialOrd {
    array: Vec<T>,
}

impl<T> SortedVector<T> where T: PartialOrd {
    fn new() -> SortedVector<T> {
        SortedVector { array: vec![] }
    }

    fn push(&mut self, value: T) {
        self.array.push(value);
        let mut index = self.array.len() - 1;
        while index > 0 {
            index -= 1;
            if self.array[index] <= self.array[index + 1] {
                break
            }
            self.array.swap(index, index + 1);
        }
    }

    fn peek(&self) -> Option<&T>   { self.array.last()     }
    fn pop(&mut self) -> Option<T> { self.array.pop()      }
    fn clear(&mut self)            { self.array.clear()    }

    #[allow(dead_code)]
    fn is_empty(&self) -> bool     { self.array.is_empty() }
}

// Queued endpoints

#[derive(PartialEq)]
struct QueuedEndpoint {
    point_index: PointIndex,
    y: f32,
}

impl Eq for QueuedEndpoint {}

impl PartialOrd<QueuedEndpoint> for QueuedEndpoint {
    fn partial_cmp(&self, other: &QueuedEndpoint) -> Option<Ordering> {
        // NB: Reversed!
        (other.y, other.point_index).partial_cmp(&(self.y, self.point_index))
    }
}

// Active edges

#[derive(Clone, PartialEq, Debug)]
struct ActiveEdge {
    segment: Segment,
    // TODO(pcwalton): Shrink `crossing` down to just one f32?
    crossing: Point2DF32,
}

impl ActiveEdge {
    fn from_segment(segment: &Segment) -> ActiveEdge {
        let crossing = if segment.baseline.from_y() < segment.baseline.to_y() {
            segment.baseline.from()
        } else {
            segment.baseline.to()
        };
        ActiveEdge::from_segment_and_crossing(segment, &crossing)
    }

    fn from_segment_and_crossing(segment: &Segment, crossing: &Point2DF32) -> ActiveEdge {
        ActiveEdge { segment: *segment, crossing: *crossing }
    }

    fn process(&mut self, built_object: &mut BuiltObject, tile_y: i16) {
        let tile_bottom = ((i32::from(tile_y) + 1) * TILE_HEIGHT as i32) as f32;
        // println!("process_active_edge({:#?}, tile_y={}({}))", self, tile_y, tile_bottom);

        let mut segment = self.segment;
        let winding = segment.baseline.y_winding();

        if segment.is_line() {
            let line_segment = segment.as_line_segment();
            self.segment = match self.process_line_segment(&line_segment, built_object, tile_y) {
                Some(lower_part) => Segment::line(&lower_part),
                None => Segment::none(),
            };
            return;
        }

        // TODO(pcwalton): Don't degree elevate!
        if !segment.is_cubic() {
            segment = segment.to_cubic();
        }

        // If necessary, draw initial line.
        if self.crossing.y() < segment.baseline.min_y() {
            let first_line_segment =
                LineSegmentF32::new(&self.crossing,
                                    &segment.baseline.upper_point()).orient(winding);
            if self.process_line_segment(&first_line_segment, built_object, tile_y).is_some() {
                return;
            }
        }

        loop {
            let rest_segment = match segment.orient(winding).as_cubic_segment().flatten_once() {
                None => {
                    let line_segment = segment.baseline;
                    self.segment = match self.process_line_segment(&line_segment,
                                                                   built_object,
                                                                   tile_y) {
                        Some(ref lower_part) => Segment::line(lower_part),
                        None => Segment::none(),
                    };
                    return;
                }
                Some(rest_segment) => rest_segment.orient(winding),
            };

            debug_assert!(segment.baseline.min_y() <= tile_bottom);

            let line_segment =
                LineSegmentF32::new(&segment.baseline.upper_point(),
                                    &rest_segment.baseline.upper_point()).orient(winding);
            if self.process_line_segment(&line_segment, built_object, tile_y).is_some() {
                self.segment = rest_segment;
                return;
            }

            segment = rest_segment;
        }
    }

    fn process_line_segment(&mut self,
                            line_segment: &LineSegmentF32,
                            built_object: &mut BuiltObject,
                            tile_y: i16)
                            -> Option<LineSegmentF32> {
        let tile_bottom = ((i32::from(tile_y) + 1) * TILE_HEIGHT as i32) as f32;
        if line_segment.max_y() <= tile_bottom {
            built_object.generate_fill_primitives_for_line(*line_segment, tile_y);
            return None;
        }

        let (upper_part, lower_part) = line_segment.split_at_y(tile_bottom);
        built_object.generate_fill_primitives_for_line(upper_part, tile_y);
        self.crossing = lower_part.upper_point();
        Some(lower_part)
    }
}

impl PartialOrd<ActiveEdge> for ActiveEdge {
    fn partial_cmp(&self, other: &ActiveEdge) -> Option<Ordering> {
        self.crossing.x().partial_cmp(&other.crossing.x())
    }
}

// Geometry

// Affine transforms

#[derive(Clone, Copy)]
struct Transform2DF32 {
    // Row-major order.
    matrix: <Sse41 as Simd>::Vf32,
    vector: Point2DF32,
}

impl Default for Transform2DF32 {
    fn default() -> Transform2DF32 {
        unsafe {
            let mut matrix = <Sse41 as Simd>::setzero_ps();
            matrix[0] = 1.0;
            matrix[3] = 1.0;
            Transform2DF32 { matrix, vector: Point2DF32::default() }
        }
    }
}

impl Transform2DF32 {
    fn from_scale(scale: &Point2DF32) -> Transform2DF32 {
        unsafe {
            let mut matrix = Sse41::setzero_ps();
            matrix[0] = scale.x();
            matrix[3] = scale.y();
            Transform2DF32 { matrix, vector: Point2DF32::default() }
        }
    }

    fn row_major(m11: f32, m12: f32, m21: f32, m22: f32, m31: f32, m32: f32) -> Transform2DF32 {
        unsafe {
            let mut matrix = Sse41::setzero_ps();
            matrix[0] = m11;
            matrix[1] = m12;
            matrix[2] = m21;
            matrix[3] = m22;
            Transform2DF32 { matrix, vector: Point2DF32::new(m31, m32) }
        }
    }

    fn transform_point(&self, point: &Point2DF32) -> Point2DF32 {
        unsafe {
            let xxyy = Sse41::shuffle_ps(point.0, point.0, 0b0101_0000);
            let x11_x12_y21_y22 = Sse41::mul_ps(xxyy, self.matrix);
            let y21_y22 = Sse41::shuffle_ps(x11_x12_y21_y22, x11_x12_y21_y22, 0b0000_1110);
            Point2DF32(Sse41::add_ps(Sse41::add_ps(x11_x12_y21_y22, y21_y22), self.vector.0))
        }
    }

    fn post_mul(&self, other: &Transform2DF32) -> Transform2DF32 {
        unsafe {
            // Here `a` is self and `b` is `other`.
            let a11a21a11a21 = Sse41::shuffle_ps(self.matrix, self.matrix, 0b1000_1000);
            let b11b11b12b12 = Sse41::shuffle_ps(other.matrix, other.matrix, 0b0101_0000);
            let lhs = Sse41::mul_ps(a11a21a11a21, b11b11b12b12);

            let a12a22a12a22 = Sse41::shuffle_ps(self.matrix, self.matrix, 0b1101_1101);
            let b21b21b22b22 = Sse41::shuffle_ps(other.matrix, other.matrix, 0b1111_1010);
            let rhs = Sse41::mul_ps(a12a22a12a22, b21b21b22b22);

            let matrix = Sse41::add_ps(lhs, rhs);
            let vector = other.transform_point(&self.vector) + other.vector;
            Transform2DF32 { matrix, vector }
        }
    }

    fn pre_mul(&self, other: &Transform2DF32) -> Transform2DF32 {
        other.post_mul(self)
    }
}

// SIMD extensions

trait SimdExt: Simd {
    // TODO(pcwalton): Default scalar implementation.
    unsafe fn shuffle_epi8(a: Self::Vi32, b: Self::Vi32) -> Self::Vi32;
}

impl SimdExt for Sse41 {
    #[inline(always)]
    unsafe fn shuffle_epi8(a: Self::Vi32, b: Self::Vi32) -> Self::Vi32 {
        I32x4_41(x86_64::_mm_shuffle_epi8(a.0, b.0))
    }
}

// Testing

#[cfg(test)]
mod test {
    use crate::SortedVector;
    use quickcheck;

    #[test]
    fn test_sorted_vec() {
        quickcheck::quickcheck(prop_sorted_vec as fn(Vec<i32>) -> bool);

        fn prop_sorted_vec(mut values: Vec<i32>) -> bool {
            let mut sorted_vec = SortedVector::new();
            for &value in &values {
                sorted_vec.push(value)
            }

            values.sort();
            let mut results = Vec::with_capacity(values.len());
            while !sorted_vec.is_empty() {
                results.push(sorted_vec.pop().unwrap());
            }
            results.reverse();
            assert_eq!(&values, &results);

            true
        }
    }
}
