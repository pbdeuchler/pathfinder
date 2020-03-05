#version 330

// pathfinder/shaders/tile_filter.vs.glsl
//
// Copyright © 2020 The Pathfinder Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

precision highp float;

uniform mat4 uTransform;
uniform vec2 uTileSize;
uniform vec2 uSrcSize;

in ivec2 aTilePosition;

out vec2 vTexCoord;

void main() {
    vec2 position = vec2(aTilePosition) * uTileSize;

    vTexCoord = position / uSrcSize;
    gl_Position = uTransform * vec4(position, 0.0, 1.0);
}