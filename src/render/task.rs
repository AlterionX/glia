use std::{borrow::Cow, cell::RefCell, mem::MaybeUninit, sync::Mutex};

use na::{Matrix3, Matrix4, UnitVector3, Vector3, Vector4};
use vulkano::format::ClearColorValue;

use crate::model::{self, camera::Camera, geom::{tri::TriMeshGeom, FMat, MeshAlloc, VMat}, AffineTransform};

#[derive(Debug, Clone)]
pub struct RenderTask<'a> {
    pub draw_wireframe: bool,
    pub cam: Camera,
    pub draws: Vec<DrawTask<'a>>,
    pub clear_color: ClearColorValue,
    pub lights: LightCollection,
}

#[derive(Debug, Clone)]
pub struct LightCollection(pub Vec<Light>);

impl LightCollection {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buffer = vec![0u8; self.0.len() * Light::bytes()];
        for (idx, light) in self.0.iter().enumerate() {
            let start = idx * Light::bytes();
            let end = start + Light::bytes();
            light.write_as_bytes_to(&mut buffer[start..end]);
        }
        buffer
    }
}

#[derive(Debug, Clone)]
pub enum Light {
    Directional(DirectionalLight),
    Point(PointLight),
}

impl Light {
    fn bytes() -> usize {
        32
    }

    // TODO Figure out how to do this properly.
    fn write_as_bytes_to(&self, buffer: &mut [u8]) {
        match self {
            Self::Point(plight) => {
                buffer[0..12].copy_from_slice(bytemuck::cast_slice(plight.color.as_slice()));
                buffer[12..16].copy_from_slice(bytemuck::cast_slice(PointLight::identifier().as_slice()));
                buffer[16..28].copy_from_slice(bytemuck::cast_slice(plight.position.as_slice()));
                buffer[28..32].copy_from_slice(&[0, 0, 0, 0]);
            },
            Self::Directional(dlight) => {
                buffer[0..12].copy_from_slice(bytemuck::cast_slice(dlight.color.as_slice()));
                buffer[12..16].copy_from_slice(bytemuck::cast_slice(DirectionalLight::identifier().as_slice()));
                buffer[16..28].copy_from_slice(bytemuck::cast_slice(dlight.direction.as_slice()));
                buffer[28..32].copy_from_slice(&[0, 0, 0, 0]);
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct PointLight {
    pub color: Vector3<f32>,
    pub position: Vector3<f32>,
}

impl PointLight {
    fn identifier() -> [f32; 1] {
        [1.]
    }
}

#[derive(Debug, Clone)]
pub struct DirectionalLight {
    pub color: Vector3<f32>,
    pub direction: UnitVector3<f32>,
}

impl DirectionalLight {
    fn identifier() -> [f32; 1] {
        [2.]
    }
}

#[derive(Debug, Clone)]
pub struct FontAtlas {
    letters_cache: RefCell<[Option<&'static TriMeshGeom>; 63]>,
    texture: Cow<'static, str>,
}

impl FontAtlas {
    const TEXTURE_FILE: &'static str = "resources/font_atlas.png";
    pub fn new() -> Self {
        Self {
            letters_cache: RefCell::new([None; 63]),
            texture: Self::TEXTURE_FILE.into(),
        }
    }

    /// Assumes that the font atlas is a simple A-Z a-z 0-9 font atlas with one blank square
    /// arranged in a 3x21 configuration (3 rows, 21 columns).
    fn idx_for_char(&self, c: char) -> u32 {
        // 26 + 26 + 10 + 1 chars -> 63 entries
        // 3 rows of 21 each
        let c_u32: u32 = c.into();
        let ref_capital_a_u32: u32 = 'A'.into();
        let ref_capital_z_u32: u32 = 'Z'.into();
        let ref_a_u32: u32 = 'a'.into();
        let ref_z_u32: u32 = 'z'.into();
        let ref_0_u32: u32 = '0'.into();
        let ref_9_u32: u32 = '9'.into();
        if (ref_capital_a_u32..=ref_capital_z_u32).contains(&c_u32) {
            c_u32 - ref_capital_a_u32
        } else if (ref_a_u32..=ref_z_u32).contains(&c_u32) {
            26 + c_u32 - ref_a_u32
        } else if (ref_0_u32..=ref_9_u32).contains(&c_u32) {
            26 + 26 + c_u32 - ref_0_u32
        } else {
            26 + 26 + 10
        }
    }

    fn char_for_idx(&self, c_idx: u32) -> char {
        // 26 + 26 + 10 + 1 chars -> 63 entries
        // 3 rows of 21 each
        let c: u32 = if (0..26).contains(&c_idx) {
            'A' as u32 + c_idx
        } else if (26..52).contains(&c_idx) {
            'a' as u32 + c_idx - 26
        } else if (52..62).contains(&c_idx) {
            '0' as u32 + c_idx - 26 - 26
        } else {
            ' ' as u32
        };
        char::from_u32(c).unwrap()
    }

    fn uv_for_char(&self, c: char) -> SpriteUV {
        let ordinal = self.idx_for_char(c);
        let (atlas_row, atlas_col): (f32, f32) = (
            (ordinal / 21) as f32,
            (ordinal % 21) as f32,
        );
        let atlas_coord_u_scale = 1f32 / 21.;
        let atlas_coord_v_scale = 1f32 / 3.;

        let u = atlas_col * atlas_coord_u_scale;
        let v = atlas_row * atlas_coord_v_scale;

        // This is intended to match the `vertical_plane` coordinates (y is inverted).
        SpriteUV([
            [u + atlas_coord_u_scale, v                      ],
            [u                      , v                      ],
            [u                      , v + atlas_coord_v_scale],
            [u + atlas_coord_u_scale, v + atlas_coord_v_scale],
        ])
    }

    fn mesh_by_ord(&self, ord: u32, alloc: &mut MeshAlloc) -> &'static TriMeshGeom {
        let c = self.char_for_idx(ord);

        self.letters_cache.borrow_mut()[ord as usize].get_or_insert_with(|| {
            let uvs = self.uv_for_char(c);
            let temp_model = Box::leak(Box::new(
                model::vertical_plane(alloc, Some(Self::TEXTURE_FILE.to_owned()), uvs.0)
            ));
            &*temp_model
        })
    }
}

#[derive(Debug, Clone)]
pub struct DrawTask<'a> {
    pub mesh: Cow<'a, TriMeshGeom>,
    pub instancing_information: Vec<Cow<'a, AffineTransform>>,
}

#[derive(Debug, Clone)]
pub struct SpriteUV([[f32; 2]; 4]);

impl DrawTask<'static> {
    pub fn texts_as_draw_tasks(font_atlas: &FontAtlas, texts: Vec<(AffineTransform, String)>, alloc: &mut MeshAlloc) -> Vec<DrawTask<'static>> {
        const EMPTY_VEC: Vec<Cow<'static, AffineTransform>> = vec![];
        let mut buffer = [EMPTY_VEC; 63];
        for (offset, text) in texts {
            for (leftward_offset, c) in text.chars().enumerate() {
                let mut actual_offset = offset.clone();
                let total_additional_offset = actual_offset.mat() * Vector4::new(leftward_offset as f32, 0., 0., 1.);
                actual_offset.pos += Vector3::new(total_additional_offset[0], total_additional_offset[1], total_additional_offset[2]) / total_additional_offset[3];
                buffer[font_atlas.idx_for_char(c) as usize].push(Cow::Owned(actual_offset));
            }
        }
        buffer.into_iter().enumerate().filter(|(_, a)| !a.is_empty()).map(|(i, v)| {
            let mesh = font_atlas.mesh_by_ord(i as u32, alloc);
            Self {
                mesh: Cow::Borrowed(mesh),
                instancing_information: v,
            }
        }).collect()
    }
}

impl<'a> RenderTask<'a> {
    pub fn instancing_information_bytes(&self) -> Vec<Matrix4<f32>> {
        // TODO Figure out if I can do this better.
        self.draws.iter()
            .flat_map(|draw| {
                draw.instancing_information.iter()
                .map(|transform| transform.mat())
            })
            .collect()
    }
}
