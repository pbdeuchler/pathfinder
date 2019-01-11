// pathfinder/geometry/src/lib.rs
//
// Copyright © 2019 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Utilities for Bézier curves.
//!
//! These may be merged into upstream Lyon eventually.

use simdeez::sse41::Sse41;

// TODO(pcwalton): Make this configurable.
pub type SimdImpl = Sse41;

pub mod clip;
pub mod cubic_to_quadratic;
pub mod line_segment;
pub mod normals;
pub mod orientation;
pub mod point;
pub mod segments;
pub mod stroke;
pub mod transform;
pub mod util;
