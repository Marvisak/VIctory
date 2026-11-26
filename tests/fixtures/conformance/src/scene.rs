//! The GPU frame's scene: a ground plane,
//! two cubes and a glowing pyramid, lit by one directional light, textured
//! from a generated checkerboard, then blurred by a compute pass. Plain
//! data and arithmetic, shared by the conformance title (which draws it
//! through VIctory) and its harness (which draws it natively on WARP from
//! equivalent HLSL), so both draw exactly the same thing.

/// The frame's size in pixels.
pub const WIDTH: u32 = 512;
pub const HEIGHT: u32 = 288;
/// The compute pass's thread group (8 x 8 pixels).
pub const GROUP: u32 = 8;
/// The texture's size in texels (RGBA8).
pub const TEXTURE: u32 = 64;

/// RGBA8 values, each a whole number of 255ths so that they survive the
/// render target's conversion exactly.
pub const CLEAR: [u8; 4] = [40, 48, 64, 255];
/// The pyramid's emitted colour (it reflects nothing).
pub const GLOW: [u8; 4] = [255, 170, 40, 255];
/// Pixels of the checked frame: one inside the pyramid where it is
/// nearest (4 pixels from any edge), and one inside its outline where the
/// nearer cube (drawn after it) hides it.
pub const GLOW_PIXEL: (u32, u32) = (252, 143);
pub const HIDDEN_PIXEL: (u32, u32) = (235, 150);

/// A vertex as the input layout describes it: position (`POSITION`, three
/// floats at 0), normal (`NORMAL`, three at 12), texture coordinates
/// (`TEXCOORD`, two at 24).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}
pub const STRIDE: u32 = 32;

/// A draw: its indices in the index buffer and its first vertex.
#[derive(Clone, Copy, Debug)]
pub struct Draw {
    pub first_index: u32,
    pub indices: u32,
    pub base_vertex: i32,
}

/// All meshes in one vertex and one index buffer (16-bit indices).
pub struct Geometry {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u16>,
    /// Ground, cube, pyramid, and the terrain beyond the ground (drawn
    /// only in the demo's window, never in the checked frame).
    pub meshes: [Draw; 4],
}

type V3 = [f32; 3];

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    [a[0] / l, a[1] / l, a[2] / l]
}

fn add(a: V3, b: V3) -> V3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Screen right as seen by a viewer looking at a face with outward normal
/// `n` (left-handed: x right, y up, z into the screen), and screen up.
fn face_axes(n: V3) -> (V3, V3) {
    let up = if n[1].abs() > 0.5 {
        [0.0, 0.0, 1.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    (cross(up, scale(n, -1.0)), up)
}

impl Geometry {
    fn push_quad(&mut self, centre: V3, n: V3, half: f32, uv_scale: f32) {
        let (right, up) = face_axes(n);
        let base = self.vertices.len() as u16;
        // Top left, top right, bottom right, bottom left: clockwise as seen
        // from outside, the front face in D3D's default winding.
        for (a, b) in [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
            let p = add(centre, add(scale(right, a * half), scale(up, b * half)));
            self.vertices.push(Vertex {
                position: p,
                normal: n,
                uv: [(a + 1.0) * 0.5 * uv_scale, (1.0 - b) * 0.5 * uv_scale],
            });
        }
        self.indices.extend([0, 1, 2, 0, 2, 3].map(|i| base + i));
    }

    /// Hills and mountains around the ground: a heightfield of Perlin
    /// noise over x in -48..48 and z in -12..70, flat (just under the
    /// ground) beneath and beside it, rising with distance from its edges
    /// into a valley of ridges.
    fn push_terrain(&mut self) {
        const COLUMNS: u16 = 160;
        const ROWS: u16 = 128;
        let (x0, x1, z0, z1) = (-48.0f32, 48.0f32, -12.0f32, 70.0f32);
        let noise = Perlin::new(0x5eed);
        let height = |x: f32, z: f32| {
            // How far beyond the 16 x 16 ground's far and side edges.
            let away = (z - 8.0).max(x.abs() - 8.0).max(0.0);
            let rise = smoothstep(0.0, 18.0, away);
            let peaks = 2.0 + 11.0 * (away / 55.0).min(1.0);
            -0.02 + rise * peaks * noise.ridged(x * 0.045, z * 0.045, 5).powf(1.6)
        };
        let base = self.vertices.len() as u16;
        let (dx, dz) = ((x1 - x0) / COLUMNS as f32, (z1 - z0) / ROWS as f32);
        for row in 0..=ROWS {
            for column in 0..=COLUMNS {
                let (x, z) = (x0 + column as f32 * dx, z0 + row as f32 * dz);
                // The normal from the height's slope across one cell.
                let sx = height(x + dx, z) - height(x - dx, z);
                let sz = height(x, z + dz) - height(x, z - dz);
                self.vertices.push(Vertex {
                    position: [x, height(x, z), z],
                    normal: normalize([-sx, 2.0 * dx.min(dz), -sz]),
                    // One light texel: the terrain takes its albedo only.
                    uv: [0.02, 0.02],
                });
            }
        }
        let at = |column: u16, row: u16| base + row * (COLUMNS + 1) + column;
        for row in 0..ROWS {
            for column in 0..COLUMNS {
                // As the ground's quad: far left, far right, near right, near
                // left (clockwise seen from above).
                let (a, b) = (at(column, row + 1), at(column + 1, row + 1));
                let (c, d) = (at(column + 1, row), at(column, row));
                self.indices.extend([a, b, c, a, c, d]);
            }
        }
    }

    fn mesh(&mut self, build: impl FnOnce(&mut Geometry)) -> Draw {
        let first_index = self.indices.len() as u32;
        let base_vertex = self.vertices.len() as i32;
        let mut part = Geometry {
            vertices: Vec::new(),
            indices: Vec::new(),
            meshes: self.meshes,
        };
        build(&mut part);
        self.vertices.extend(part.vertices);
        self.indices.extend(&part.indices);
        Draw {
            first_index,
            indices: part.indices.len() as u32,
            base_vertex,
        }
    }

    pub fn new() -> Geometry {
        let none = Draw {
            first_index: 0,
            indices: 0,
            base_vertex: 0,
        };
        let mut g = Geometry {
            vertices: Vec::new(),
            indices: Vec::new(),
            meshes: [none; 4],
        };
        // A 16 x 16 ground, the texture repeated 4 times across it.
        let ground = g.mesh(|g| g.push_quad([0.0; 3], [0.0, 1.0, 0.0], 8.0, 4.0));
        // A unit cube about its centre.
        let cube = g.mesh(|g| {
            for n in [
                [1.0, 0.0, 0.0],
                [-1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, -1.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 0.0, -1.0],
            ] {
                g.push_quad(scale(n, 0.5), n, 0.5, 1.0);
            }
        });
        // A pyramid of height 1 on a unit square, about its centre.
        let pyramid = g.mesh(|g| {
            g.push_quad([0.0, -0.5, 0.0], [0.0, -1.0, 0.0], 0.5, 1.0);
            for d in [
                [1.0, 0.0, 0.0],
                [-1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0],
                [0.0, 0.0, -1.0],
            ] {
                let (right, _) = face_axes(d);
                let n = normalize(add(scale(d, 2.0), [0.0, 1.0, 0.0]));
                let base = g.vertices.len() as u16;
                let corner = |side: f32| {
                    add(
                        add(scale(d, 0.5), scale(right, side * 0.5)),
                        [0.0, -0.5, 0.0],
                    )
                };
                for (p, uv) in [
                    ([0.0, 0.5, 0.0], [0.5, 0.0]),
                    (corner(1.0), [1.0, 1.0]),
                    (corner(-1.0), [0.0, 1.0]),
                ] {
                    g.vertices.push(Vertex {
                        position: p,
                        normal: n,
                        uv,
                    });
                }
                g.indices.extend([base, base + 1, base + 2]);
            }
        });
        let terrain = g.mesh(|g| g.push_terrain());
        g.meshes = [ground, cube, pyramid, terrain];
        g
    }
}

impl Default for Geometry {
    fn default() -> Self {
        Geometry::new()
    }
}

/// A 4 x 4 matrix as four rows; a point p transforms to (row i . p).
pub type Matrix = [[f32; 4]; 4];

fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[i][k] * b[k][j]).sum();
        }
    }
    m
}

/// Placement: uniform scale, then a turn about y (radians), then a move.
fn world(at: V3, size: f32, turn: f32) -> Matrix {
    let (s, c) = turn.sin_cos();
    [
        [c * size, 0.0, s * size, at[0]],
        [0.0, size, 0.0, at[1]],
        [-s * size, 0.0, c * size, at[2]],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

/// Camera: looking from `eye` at `target`, 60 degrees vertically, depth 0
/// at 0.1 and 1 at 100 (D3D's clip space).
fn view_projection(eye: V3, target: V3) -> Matrix {
    let z = normalize(add(target, scale(eye, -1.0)));
    let x = normalize(cross([0.0, 1.0, 0.0], z));
    let y = cross(z, x);
    let view = [
        [x[0], x[1], x[2], -dot(x, eye)],
        [y[0], y[1], y[2], -dot(y, eye)],
        [z[0], z[1], z[2], -dot(z, eye)],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let (near, far) = (0.1f32, 100.0f32);
    let ys = 1.0 / (30.0f32.to_radians()).tan();
    let xs = ys * HEIGHT as f32 / WIDTH as f32;
    let q = far / (far - near);
    let projection = [
        [xs, 0.0, 0.0, 0.0],
        [0.0, ys, 0.0, 0.0],
        [0.0, 0.0, q, -near * q],
        [0.0, 0.0, 1.0, 0.0],
    ];
    multiply(&projection, &view)
}

/// The per-frame constants (`b0`), 256 bytes: the view-projection rows,
/// then (each a float4) the direction towards the light, the eye, the
/// light's colour and the ambient light.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SceneConstants {
    pub view_projection: Matrix,
    pub light: [f32; 4],
    pub eye: [f32; 4],
    pub light_colour: [f32; 4],
    pub ambient: [f32; 4],
    pub _pad: [f32; 32],
}

/// The per-draw constants (`b1`), 256 bytes: the world rows, then the
/// albedo with the specular exponent in w, the specular colour and the
/// emitted colour.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MeshConstants {
    pub world: Matrix,
    pub albedo_shininess: [f32; 4],
    pub specular: [f32; 4],
    pub emissive: [f32; 4],
    pub _pad: [f32; 36],
}

const _: () = assert!(size_of::<SceneConstants>() == 256);
const _: () = assert!(size_of::<MeshConstants>() == 256);

/// The frame at time `t` seconds (0 for the checked frame): its constants
/// and, per draw, which mesh it draws and with what constants. Draw order:
/// ground, pyramid, then the cubes (the nearer cube hides part of the
/// pyramid, drawn before it: the depth test decides).
pub fn frame(t: f32) -> (SceneConstants, Vec<(usize, MeshConstants)>) {
    let eye = [0.0, 2.6, -4.6];
    let light = normalize([-0.45, 0.8, -0.4]);
    let scene = SceneConstants {
        view_projection: view_projection(eye, [0.1, 0.55, 0.7]),
        light: [light[0], light[1], light[2], 0.0],
        eye: [eye[0], eye[1], eye[2], 1.0],
        light_colour: [1.0, 0.96, 0.9, 0.0],
        ambient: [0.18, 0.2, 0.26, 0.0],
        _pad: [0.0; 32],
    };
    let mesh = |world, albedo: V3, shininess, specular: V3, emissive: V3| MeshConstants {
        world,
        albedo_shininess: [albedo[0], albedo[1], albedo[2], shininess],
        specular: [specular[0], specular[1], specular[2], 0.0],
        emissive: [emissive[0], emissive[1], emissive[2], 0.0],
        _pad: [0.0; 36],
    };
    let glow = GLOW.map(|c| f32::from(c) / 255.0);
    let draws = vec![
        (
            0,
            mesh(
                world([0.0; 3], 1.0, 0.0),
                [0.9; 3],
                16.0,
                [0.15; 3],
                [0.0; 3],
            ),
        ),
        (
            2,
            mesh(
                world([0.05, 0.55, 1.6], 1.1, 0.4 + t * 0.7),
                [0.0; 3],
                1.0,
                [0.0; 3],
                [glow[0], glow[1], glow[2]],
            ),
        ),
        (
            1,
            mesh(
                world([-1.0, 0.6, 0.0], 1.2, 0.5 + t * 0.5),
                [0.95, 0.45, 0.35],
                48.0,
                [0.8; 3],
                [0.0; 3],
            ),
        ),
        (
            1,
            mesh(
                world([1.5, 0.5, 1.2], 1.0, -0.35 - t * 0.9),
                [0.35, 0.6, 0.95],
                24.0,
                [0.5; 3],
                [0.0; 3],
            ),
        ),
    ];
    (scene, draws)
}

/// Objects the demo's stress scale adds to the frame (none in the checked
/// frame): `count` small cubes and pyramids in a grid over the ground, each
/// turning and bobbing at time `t`, each a draw with constants of its own.
pub fn crowd(t: f32, count: usize) -> Vec<(usize, MeshConstants)> {
    let side = (count as f32).sqrt().ceil().max(1.0) as usize;
    let (x0, x1, z0, z1) = (-7.0f32, 7.0f32, -1.5f32, 7.5f32);
    let (step_x, step_z) = ((x1 - x0) / side as f32, (z1 - z0) / side as f32);
    let size = (step_x.min(step_z) * 0.55).min(0.35);
    (0..count)
        .map(|i| {
            let (column, row) = ((i % side) as f32, (i / side) as f32);
            let phase = i as f32 * 0.37;
            let at = [
                x0 + (column + 0.5) * step_x,
                size * 0.5 + size * 0.3 * (t * 2.0 + phase).sin().abs(),
                z0 + (row + 0.5) * step_z,
            ];
            let turn = phase + t * (0.4 + (i % 7) as f32 * 0.15);
            let albedo = hue(i as f32 * 0.618_034 % 1.0);
            let constants = MeshConstants {
                world: world(at, size, turn),
                albedo_shininess: [albedo[0], albedo[1], albedo[2], 24.0],
                specular: [0.4, 0.4, 0.4, 0.0],
                emissive: [0.0; 4],
                _pad: [0.0; 36],
            };
            (1 + i % 2, constants)
        })
        .collect()
}

/// The terrain's constants: where it lies (as built) and its colour.
pub fn terrain() -> MeshConstants {
    MeshConstants {
        world: world([0.0; 3], 1.0, 0.0),
        albedo_shininess: [0.36, 0.5, 0.3, 6.0],
        specular: [0.04, 0.04, 0.04, 0.0],
        emissive: [0.0; 4],
        _pad: [0.0; 36],
    }
}

fn smoothstep(from: f32, to: f32, x: f32) -> f32 {
    let t = ((x - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Ken Perlin's improved gradient noise in two dimensions, with its
/// permutation shuffled from `seed` (the same terrain every run).
struct Perlin {
    p: [u8; 512],
}

impl Perlin {
    fn new(seed: u64) -> Perlin {
        let mut table: [u8; 256] = std::array::from_fn(|i| i as u8);
        let mut state = seed;
        for i in (1..256).rev() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            table.swap(i, (state >> 33) as usize % (i + 1));
        }
        Perlin {
            p: std::array::from_fn(|i| table[i % 256]),
        }
    }

    /// Noise at (x, y), about -1 to 1.
    fn noise(&self, x: f32, y: f32) -> f32 {
        let (xf, yf) = (x.floor(), y.floor());
        let (xi, yi) = ((xf as i32 & 255) as usize, (yf as i32 & 255) as usize);
        let (x, y) = (x - xf, y - yf);
        let fade = |t: f32| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
        let grad = |hash: u8, x: f32, y: f32| match hash & 7 {
            0 => x + y,
            1 => -x + y,
            2 => x - y,
            3 => -x - y,
            4 => x,
            5 => -x,
            6 => y,
            _ => -y,
        };
        let p = &self.p;
        let corner = |dx: usize, dy: usize| p[p[xi + dx] as usize + yi + dy];
        let (u, v) = (fade(x), fade(y));
        let lerp = |a: f32, b: f32, t: f32| a + t * (b - a);
        lerp(
            lerp(grad(corner(0, 0), x, y), grad(corner(1, 0), x - 1.0, y), u),
            lerp(
                grad(corner(0, 1), x, y - 1.0),
                grad(corner(1, 1), x - 1.0, y - 1.0),
                u,
            ),
            v,
        )
    }

    /// Ridged multifractal noise of `octaves` octaves, 0 to 1: sharp crests
    /// where the noise crosses zero, as mountain ridges.
    fn ridged(&self, x: f32, y: f32, octaves: u32) -> f32 {
        let (mut sum, mut amplitude, mut frequency, mut total) = (0.0, 1.0, 1.0, 0.0);
        for _ in 0..octaves {
            let ridge = 1.0 - self.noise(x * frequency, y * frequency).abs();
            sum += amplitude * ridge * ridge;
            total += amplitude;
            amplitude *= 0.5;
            frequency *= 2.0;
        }
        sum / total
    }
}

/// A saturated colour of hue `h` (0 to 1).
fn hue(h: f32) -> V3 {
    let channel = |offset: f32| {
        let k = (h * 6.0 + offset) % 6.0;
        0.9 - 0.6 * (k.min(4.0 - k).clamp(0.0, 1.0))
    };
    [channel(5.0), channel(3.0), channel(1.0)]
}

/// The texture's texels, rows of RGBA8: a checkerboard of 8-texel cells,
/// light cells and tinted dark ones.
pub fn texels() -> Vec<u8> {
    let mut out = Vec::with_capacity((TEXTURE * TEXTURE * 4) as usize);
    for y in 0..TEXTURE {
        for x in 0..TEXTURE {
            let (cx, cy) = (x / 8, y / 8);
            let texel = if (cx + cy) % 2 == 0 {
                [235, 230, 215, 255]
            } else {
                [
                    60 + 20 * (cx % 4) as u8,
                    100 + 15 * (cy % 4) as u8,
                    170,
                    255,
                ]
            };
            out.extend(texel);
        }
    }
    out
}
