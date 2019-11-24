use crate::backend::{self, render_buffer::*};
use crate::blob::*;
use crate::buffer::Buffer;
use crate::gpu_debugger;
use crate::gpu_profiler;
use crate::texture::{Texture, TextureKey};
use crate::vulkan::*;
use ash::version::{DeviceV1_0, EntryV1_0, InstanceV1_0, InstanceV1_1};
use ash::{vk, Device};
use relative_path::{RelativePath, RelativePathBuf};
use shader_prepper;
use snoozy::futures::future::{try_join_all, BoxFuture, FutureExt};
use snoozy::*;
use std::collections::HashMap;
use std::ffi::CStr;

macro_rules! def_shader_uniform_types {
    (@resolved_type SnoozyRef<ShaderUniformBundle>) => {
        ResolvedShaderUniformBundle
    };
    (@resolved_type SnoozyRef<$t:ty>) => {
        $t
    };
    (@resolved_type ShaderUniformBundle) => {
        ResolvedShaderUniformBundle
    };
    (@resolved_type $t:ty) => {
        $t
    };
    (@resolve $ctx:ident SnoozyRef<ShaderUniformBundle>, $v:ident) => {
        resolve($ctx.clone(), (*$ctx.get($v).await?).clone()).await?
    };
    (@resolve $ctx:ident SnoozyRef<$t:ty>, $v:ident) => {
        (*$ctx.get($v).await?).clone()
    };
    (@resolve $ctx:ident ShaderUniformBundle, $v:ident) => {
        resolve($ctx.clone(), $v.clone()).await?
    };
    (@resolve $ctx:ident $t:ty, $v:ident) => {
        (*$v).clone()
    };
    ($($name:ident($($type:tt)*)),* $(,)*) => {
		#[derive(Clone, Debug, Serialize)]
		pub enum ShaderUniformValue {
			$($name($($type)*)),*
		}

		pub enum ResolvedShaderUniformValue {
			$($name(def_shader_uniform_types!(@resolved_type $($type)*))),*
		}

        impl ShaderUniformValue {
            pub fn resolve<'a>(&'a self, ctx: Context) -> BoxFuture<'a, Result<ResolvedShaderUniformValue>> {
                async move {
                    match self {
                        $(ShaderUniformValue::$name(v) => Ok(ResolvedShaderUniformValue::$name(
                            def_shader_uniform_types!(@resolve ctx $($type)*, v)
                        ))),*
                    }
                }.boxed()
            }
		}

        $(
			impl From<$($type)*> for ShaderUniformValue {
				fn from(v: $($type)*) -> ShaderUniformValue {
					ShaderUniformValue::$name(v)
				}
			}
		)*
	}
}

def_shader_uniform_types! {
    Float32(f32),
    Uint32(u32),
    Int32(i32),
    Ivec2((i32, i32)),
    Bundle(ShaderUniformBundle),
    Float32Asset(SnoozyRef<f32>),
    Uint32Asset(SnoozyRef<u32>),
    UsizeAsset(SnoozyRef<usize>),
    TextureAsset(SnoozyRef<Texture>),
    BufferAsset(SnoozyRef<Buffer>),
    BundleAsset(SnoozyRef<ShaderUniformBundle>),
}

#[derive(Debug, Serialize, Clone)]
pub struct ShaderUniformHolder {
    name: String,
    value: ShaderUniformValue,
    shallow_hash: u64,
}

use std::hash::{Hash, Hasher};
impl Hash for ShaderUniformHolder {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.shallow_hash.hash(state);
    }
}

pub struct ResolvedShaderUniformHolder {
    name: String,
    value: ResolvedShaderUniformValue,
}

impl ShaderUniformHolder {
    pub fn new<T: Into<ShaderUniformValue> + 'static>(name: &str, value: T) -> ShaderUniformHolder {
        Self::from_name_value(name, value.into())
    }

    pub fn from_name_value(name: &str, value: ShaderUniformValue) -> ShaderUniformHolder {
        let mut s = DefaultSnoozyHash::default();
        whatever_hash(&value, &mut s);
        let shallow_hash = std::hash::Hasher::finish(&mut s);

        ShaderUniformHolder {
            name: name.to_string(),
            value,
            shallow_hash,
        }
    }

    pub async fn resolve(&self, ctx: Context) -> Result<ResolvedShaderUniformHolder> {
        Ok(ResolvedShaderUniformHolder {
            name: self.name.clone(),
            value: self.value.resolve(ctx.clone()).await?,
        })
    }
}

pub type ShaderUniformBundle = Vec<ShaderUniformHolder>;
pub type ResolvedShaderUniformBundle = Vec<ResolvedShaderUniformHolder>;

async fn resolve(
    ctx: Context,
    uniforms: Vec<ShaderUniformHolder>,
) -> Result<Vec<ResolvedShaderUniformHolder>> {
    // TODO: don't clone all the things.
    //
    try_join_all(uniforms.into_iter().map(|u| {
        let ctx = ctx.clone();
        tokio::executor::Executor::spawn_with_handle(
            &mut tokio::executor::DefaultExecutor::current(),
            async move { u.resolve(ctx).await },
        )
        .expect("failed to spawn_with_handle()")
    }))
    .await
}

#[macro_export]
macro_rules! shader_uniforms {
    (@parse_name $name:ident) => {
        stringify!($name)
    };
    (@parse_name) => {
        ""
    };
    ($($($name:ident)? : $value:expr),* $(,)*) => {
        vec![
            $(ShaderUniformHolder::new(shader_uniforms!(@parse_name $($name)?), $value),)*
        ]
    }
}

#[macro_export]
macro_rules! shader_uniform_bundle {
    (@parse_name $name:ident) => {
        stringify!($name)
    };
    (@parse_name) => {
        ""
    };
    ($($($name:ident)? : $value:expr),* $(,)*) => {
        ShaderUniformHolder::new("",
        vec![
            $(ShaderUniformHolder::new(shader_uniforms!(@parse_name $($name)?), $value),)*
        ])
    }
}

pub struct ComputeShader {
    pub name: String,
    handle: u32,
    reflection: ShaderReflection,
}

#[derive(Debug)]
pub struct ShaderUniformReflection {
    pub location: i32,
    pub gl_type: u32,
}

#[derive(Debug)]
pub struct ShaderReflection {
    uniforms: HashMap<String, ShaderUniformReflection>,
}

fn reflect_shader(gfx: &crate::Gfx, program_handle: u32) -> ShaderReflection {
    unimplemented!()
    /*let mut uniform_count = 0i32;
    unsafe {
        gl.GetProgramiv(program_handle, gl::ACTIVE_UNIFORMS, &mut uniform_count);
    }

    let uniforms: HashMap<String, ShaderUniformReflection> = (0..uniform_count)
        .map(|index| unsafe {
            let mut name_len = 0;
            let mut gl_type = 0;
            let mut gl_size = 0;

            let mut name_str: Vec<u8> = vec![b'\0'; 128];
            gl.GetActiveUniform(
                program_handle,
                index as u32,
                127,
                &mut name_len,
                &mut gl_size,
                &mut gl_type,
                name_str.as_mut_ptr() as *mut i8,
            );

            let location = gl.GetUniformLocation(program_handle, name_str.as_ptr() as *const i8);

            let name = CStr::from_ptr(name_str.as_ptr() as *const i8)
                .to_string_lossy()
                .into_owned();
            let refl = ShaderUniformReflection { location, gl_type };

            (name, refl)
        })
        .collect();

    ShaderReflection { uniforms }*/
}

impl Drop for ComputeShader {
    fn drop(&mut self) {
        // TODO: defer
        /*unsafe {
            gl.DeleteProgram(self.handle);
        }*/
    }
}

struct ShaderIncludeProvider {
    ctx: Context,
}

impl<'a> shader_prepper::IncludeProvider for ShaderIncludeProvider {
    type IncludeContext = AssetPath;

    fn get_include(
        &mut self,
        path: &str,
        include_context: &Self::IncludeContext,
    ) -> Result<(String, Self::IncludeContext)> {
        let asset_path: AssetPath = if let Some(crate_end) = path.find("::") {
            let crate_name = path.chars().take(crate_end).collect();
            let asset_name = path.chars().skip(crate_end + 2).collect();

            AssetPath {
                crate_name,
                asset_name,
            }
        } else {
            if let Some('/') = path.chars().next() {
                AssetPath {
                    crate_name: include_context.crate_name.clone(),
                    asset_name: path.chars().skip(1).collect(),
                }
            } else {
                let mut folder: RelativePathBuf = include_context.asset_name.clone().into();
                folder.pop();
                AssetPath {
                    crate_name: include_context.crate_name.clone(),
                    asset_name: folder.join(path).as_str().to_string(),
                }
            }
        };

        RelativePath::new(path);
        let blob =
            snoozy::futures::executor::block_on(self.ctx.get(&load_blob(asset_path.clone())))?;
        String::from_utf8(blob.contents.clone())
            .map_err(|e| format_err!("{}", e))
            .map(|ok| (ok, asset_path))
    }
}

fn get_shader_text(source: &[shader_prepper::SourceChunk]) -> String {
    let preamble = "#version 430\n".to_string();

    let mod_sources = source.iter().enumerate().map(|(i, s)| {
        let s = format!("#line 0 {}\n", i + 1) + &s.source;
        s
    });
    let mod_sources = std::iter::once(preamble).chain(mod_sources);
    let mod_sources: Vec<_> = mod_sources.collect();

    mod_sources.join("")
}

fn shaderc_compile_glsl(source: &[shader_prepper::SourceChunk]) -> shaderc::CompilationArtifact {
    use shaderc;
    let source = get_shader_text(source);

    let mut compiler = shaderc::Compiler::new().unwrap();
    let mut options = shaderc::CompileOptions::new().unwrap();
    options.add_macro_definition("EP", Some("main"));
    let binary_result = compiler
        .compile_into_spirv(
            &source,
            shaderc::ShaderKind::Compute,
            "shader.glsl",
            "main",
            Some(&options),
        )
        .unwrap();

    assert_eq!(Some(&0x07230203), binary_result.as_binary().first());

    binary_result
}

struct ComputePipeline {
    pub pipeline_layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
}

fn convert_spirv_reflect_err<T>(res: std::result::Result<T, &'static str>) -> Result<T> {
    match res {
        Ok(res) => Ok(res),
        Err(e) => bail!("SPIR-V reflection error: {}", e),
    }
}

fn reflect_spirv_shader(shader_code: &[u32]) -> Result<spirv_reflect::ShaderModule> {
    convert_spirv_reflect_err(spirv_reflect::ShaderModule::load_u32_data(shader_code))
}

fn generate_descriptor_set_layout(
    refl: &spirv_reflect::ShaderModule,
) -> std::result::Result<vk::DescriptorSetLayout, &'static str> {
    let mut binding_flags: Vec<vk::DescriptorBindingFlagsEXT> = Vec::new();
    let mut bindings: Vec<vk::DescriptorSetLayoutBinding> = Vec::new();
    // vk::DescriptorBindingFlagsEXT::empty(),

    let entry = Some("main");
    for descriptor_set in refl.enumerate_descriptor_sets(entry)?.iter() {
        println!("descriptor set {}", descriptor_set.set);
        for binding in descriptor_set.bindings.iter() {
            use spirv_reflect::types::descriptor::ReflectDescriptorType;

            match binding.descriptor_type {
                ReflectDescriptorType::UniformBuffer => bindings.push(
                    vk::DescriptorSetLayoutBinding::builder()
                        .descriptor_count(binding.count)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                        .binding(binding.binding)
                        .build(),
                ),
                ReflectDescriptorType::StorageImage => bindings.push(
                    vk::DescriptorSetLayoutBinding::builder()
                        .descriptor_count(binding.count)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                        .binding(binding.binding)
                        .build(),
                ),
                _ => print!("\tunsupported"),
            }
        }
    }

    let mut binding_flags = vk::DescriptorSetLayoutBindingFlagsCreateInfoEXT::builder()
        .binding_flags(&binding_flags)
        .build();

    let descriptor_set_layout = unsafe {
        vk_device()
            .create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::builder()
                    .bindings(&bindings)
                    .push_next(&mut binding_flags)
                    .build(),
                None,
            )
            .unwrap()
    };

    Ok(descriptor_set_layout)
}

fn create_compute_pipeline(
    vk_device: &Device,
    descriptor_set_layout: vk::DescriptorSetLayout,
    shader_code: &[u32],
) -> Result<ComputePipeline> {
    use std::ffi::{CStr, CString};
    use std::io::Cursor;

    let shader_entry_name = CString::new("main").unwrap();

    let descriptor_set_layouts = [descriptor_set_layout];
    let layout_create_info =
        vk::PipelineLayoutCreateInfo::builder().set_layouts(&descriptor_set_layouts);

    unsafe {
        let shader_module = vk_device
            .create_shader_module(
                &vk::ShaderModuleCreateInfo::builder().code(&shader_code),
                None,
            )
            .unwrap();

        let stage_create_info = vk::PipelineShaderStageCreateInfo::builder()
            .module(shader_module)
            .stage(vk::ShaderStageFlags::COMPUTE)
            .name(&shader_entry_name);

        let pipeline_layout = vk_device
            .create_pipeline_layout(&layout_create_info, None)
            .unwrap();

        let pipeline_info = vk::ComputePipelineCreateInfo::builder()
            .stage(stage_create_info.build())
            .layout(pipeline_layout);

        // TODO: pipeline cache
        let pipeline = vk_device
            .create_compute_pipelines(vk::PipelineCache::null(), &[pipeline_info.build()], None)
            .expect("pipeline")[0];

        /*let data = ComputePipelineData {
            pipeline: pipelines[0],
            descriptor_layouts,
            layout: pipeline_layout,
        };*/

        Ok(ComputePipeline {
            pipeline_layout,
            pipeline,
        })
    }
}

#[snoozy]
pub async fn load_cs(ctx: Context, path: &AssetPath) -> Result<ComputeShader> {
    let source = shader_prepper::process_file(
        &path.asset_name,
        &mut ShaderIncludeProvider { ctx: ctx.clone() },
        AssetPath {
            crate_name: path.crate_name.clone(),
            asset_name: String::new(),
        },
    )?;

    let spirv = shaderc_compile_glsl(&source);
    let refl = reflect_spirv_shader(spirv.as_binary())?;

    let descriptor_set_layout = convert_spirv_reflect_err(generate_descriptor_set_layout(&refl))?;
    let pipeline = create_compute_pipeline(vk_device(), descriptor_set_layout, spirv.as_binary())?;

    /*with_gl(|gl| {
        let handle = backend::shader::make_shader(gfx, gl::COMPUTE_SHADER, &source)?;
        let name = std::path::Path::new(&path.asset_name)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or("unknown".to_string());

        let handle = backend::shader::make_program(gfx, &[handle])?;
        let reflection = reflect_shader(gfx, handle);

        Ok(ComputeShader {
            handle,
            name,
            reflection,
        })
    })*/
    unimplemented!()
}

#[snoozy]
pub async fn load_cs_from_string(
    _ctx: Context,
    source: &String,
    name: &String,
) -> Result<ComputeShader> {
    let source = [shader_prepper::SourceChunk {
        source: source.clone(),
        file: "no-file".to_owned(),
        line_offset: 0,
    }];

    /*with_gl(|gl| {
        let handle = backend::shader::make_shader(gfx, gl::COMPUTE_SHADER, &source)?;
        let handle = backend::shader::make_program(gfx, &[handle])?;
        let reflection = reflect_shader(gfx, handle);

        Ok(ComputeShader {
            handle,
            name: name.clone(),
            reflection,
        })
    })*/
    unimplemented!()
}

pub struct RasterSubShader {
    handle: u32,
}

impl Drop for RasterSubShader {
    fn drop(&mut self) {
        // TODO: defer
        /*unsafe {
            gl.DeleteShader(self.handle);
        }*/
    }
}

#[snoozy]
pub async fn load_vs(ctx: Context, path: &AssetPath) -> Result<RasterSubShader> {
    let source = shader_prepper::process_file(
        &path.asset_name,
        &mut ShaderIncludeProvider { ctx: ctx.clone() },
        AssetPath {
            crate_name: path.crate_name.clone(),
            asset_name: String::new(),
        },
    )?;

    /*with_gl(|gl| {
        Ok(RasterSubShader {
            handle: backend::shader::make_shader(gfx, gl::VERTEX_SHADER, &source)?,
        })
    })*/
    unimplemented!()
}

#[snoozy]
pub async fn load_ps(ctx: Context, path: &AssetPath) -> Result<RasterSubShader> {
    let source = shader_prepper::process_file(
        &path.asset_name,
        &mut ShaderIncludeProvider { ctx: ctx.clone() },
        AssetPath {
            crate_name: path.crate_name.clone(),
            asset_name: String::new(),
        },
    )?;

    /*with_gl(|gl| {
        Ok(RasterSubShader {
            handle: backend::shader::make_shader(gfx, gl::FRAGMENT_SHADER, &source)?,
        })
    })*/
    unimplemented!()
}

pub struct RasterPipeline {
    handle: u32,
    reflection: ShaderReflection,
}

#[snoozy]
pub async fn make_raster_pipeline(
    ctx: Context,
    shaders_in: &Vec<SnoozyRef<RasterSubShader>>,
) -> Result<RasterPipeline> {
    let mut shaders = Vec::with_capacity(shaders_in.len());
    for a in shaders_in.iter() {
        shaders.push(ctx.get(&*a).await?.handle);
    }

    /*with_gl(|gl| {
        let handle = backend::shader::make_program(gfx, shaders.as_slice())?;
        let reflection = reflect_shader(gfx, handle);

        Ok(RasterPipeline { handle, reflection })
    })*/
    unimplemented!()
}

#[derive(Default)]
struct ShaderUniformPlumber {
    img_unit: i32,
    ssbo_unit: u32,
    ubo_unit: u32,
    index_count: Option<u32>,
    warnings: Vec<String>,
}

pub enum PlumberEvent<'a, 'b> {
    SetUniform {
        name: &'a str,
        value: &'b ResolvedShaderUniformValue,
    },
    EnterScope,
    LeaveScope,
}

pub trait ShaderUniformPlumberCallback {
    fn plumb(&mut self, gfx: &crate::Gfx, name: &str, value: &ResolvedShaderUniformValue);
}

impl ShaderUniformPlumber {
    fn plumb_uniform(
        &mut self,
        gfx: &crate::Gfx,
        program_handle: u32,
        reflection: &ShaderReflection,
        name: &str,
        value: &ResolvedShaderUniformValue,
    ) {
        unimplemented!()
        /*let c_name = std::ffi::CString::new(name.clone()).unwrap();

        macro_rules! get_uniform_no_warn {
            () => {
                reflection.uniforms.get(name)
            };
        }

        macro_rules! get_uniform {
            () => {{
                if let Some(u) = reflection.uniforms.get(name) {
                    Some(u)
                } else {
                    self.warnings
                        .push(format!("Shader uniform not found: {}", name).to_owned());
                    None
                }
            }};
        }

        match value {
            ResolvedShaderUniformValue::Bundle(_) => {}
            ResolvedShaderUniformValue::BundleAsset(_) => {}

            ResolvedShaderUniformValue::TextureAsset(ref tex) => {
                if let Some(loc) = reflection.uniforms.get(&(name.to_owned() + "_size")) {
                    unsafe {
                        gl.Uniform4f(
                            loc.location,
                            tex.key.width as f32,
                            tex.key.height as f32,
                            1.0 / tex.key.width as f32,
                            1.0 / tex.key.height as f32,
                        );
                    }
                }

                unsafe {
                    if let Some(loc) = get_uniform!() {
                        if gl::IMAGE_2D == loc.gl_type {
                            let level = 0;
                            let layered = gl::FALSE;
                            gl.BindImageTexture(
                                self.img_unit as u32,
                                tex.texture_id,
                                level,
                                layered,
                                0,
                                gl::READ_ONLY,
                                tex.key.format,
                            );
                            gl.Uniform1i(loc.location, self.img_unit);
                            self.img_unit += 1;
                        } else if gl::SAMPLER_2D == loc.gl_type {
                            gl.ActiveTexture(gl::TEXTURE0 + self.img_unit as u32);
                            gl.BindTexture(gl::TEXTURE_2D, tex.texture_id);
                            gl.BindSampler(self.img_unit as u32, tex.sampler_id);
                            gl.Uniform1i(loc.location, self.img_unit);
                            self.img_unit += 1;
                        } else {
                            panic!("unspupported sampler type: {:x}", loc.gl_type);
                        }
                    }
                }
            }
            ResolvedShaderUniformValue::BufferAsset(ref buf) => {
                let u_block_index =
                    unsafe { gl.GetUniformBlockIndex(program_handle, c_name.as_ptr()) };

                let ss_block_index = unsafe {
                    gl.GetProgramResourceIndex(
                        program_handle,
                        gl::SHADER_STORAGE_BLOCK,
                        c_name.as_ptr(),
                    )
                };

                if u_block_index != std::u32::MAX {
                    unsafe {
                        gl.UniformBlockBinding(program_handle, u_block_index, self.ubo_unit);
                        gl.BindBufferBase(gl::UNIFORM_BUFFER, self.ubo_unit, buf.buffer_id);
                    }
                    self.ubo_unit += 1;
                } else if ss_block_index != std::u32::MAX {
                    unsafe {
                        gl.ShaderStorageBlockBinding(
                            program_handle,
                            ss_block_index,
                            self.ssbo_unit,
                        );
                        gl.BindBufferBase(gl::SHADER_STORAGE_BUFFER, self.ssbo_unit, buf.buffer_id);
                    }
                    self.ssbo_unit += 1;
                } else {
                    unsafe {
                        if let Some(loc) = get_uniform_no_warn!() {
                            if gl::SAMPLER_BUFFER == loc.gl_type
                                || gl::UNSIGNED_INT_SAMPLER_BUFFER == loc.gl_type
                                || gl::INT_SAMPLER_BUFFER == loc.gl_type
                            {
                                gl.ActiveTexture(gl::TEXTURE0 + self.img_unit as u32);
                                gl.BindTexture(
                                    gl::TEXTURE_BUFFER,
                                    buf.texture_id
                                        .expect("buffer doesn't have a texture buffer"),
                                );
                                gl.BindSampler(self.img_unit as u32, 0);
                                gl.Uniform1i(loc.location, self.img_unit);
                                self.img_unit += 1;
                            } else {
                                panic!(
                                    "Buffer textures can only be bound to gsamplerBuffer; got {:x}",
                                    loc.gl_type
                                );
                            }
                        }
                    }
                }
            }
            ResolvedShaderUniformValue::Float32(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform1f(loc.location, *value);
                }
            },
            ResolvedShaderUniformValue::Int32(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform1i(loc.location, *value);
                }
            },
            ResolvedShaderUniformValue::Uint32(value) => unsafe {
                if name == "mesh_index_count" {
                    self.index_count = Some(*value);
                } else {
                    if let Some(loc) = get_uniform!() {
                        gl.Uniform1ui(loc.location, *value);
                    }
                }
            },
            ResolvedShaderUniformValue::Ivec2(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform2i(loc.location, value.0, value.1);
                }
            },
            ResolvedShaderUniformValue::Float32Asset(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform1f(loc.location, *value);
                }
            },
            ResolvedShaderUniformValue::Uint32Asset(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform1ui(loc.location, *value);
                }
            },
            ResolvedShaderUniformValue::UsizeAsset(value) => unsafe {
                if let Some(loc) = get_uniform!() {
                    gl.Uniform1i(loc.location, *value as i32);
                }
            },
        }*/
    }

    fn plumb<UniformHandlerFn>(
        &mut self,
        gfx: &crate::Gfx,
        program_handle: u32,
        reflection: &ShaderReflection,
        uniforms: &Vec<ResolvedShaderUniformHolder>,
        uniform_handler_fn: &mut UniformHandlerFn,
    ) where
        UniformHandlerFn: FnMut(&mut dyn ShaderUniformPlumberCallback, PlumberEvent),
    {
        impl ShaderUniformPlumberCallback for (&mut ShaderUniformPlumber, u32, &ShaderReflection) {
            fn plumb(&mut self, gfx: &crate::Gfx, name: &str, value: &ResolvedShaderUniformValue) {
                self.0.plumb_uniform(gfx, self.1, self.2, name, value)
            }
        }

        macro_rules! scope_event {
            ($event_type: expr) => {
                uniform_handler_fn(&mut (&mut *self, program_handle, reflection), $event_type);
            };
        }

        // Do non-bundle values first so that they become visible to bundle handlers
        for uniform in uniforms.iter() {
            match uniform.value {
                ResolvedShaderUniformValue::Bundle(_) => {}
                ResolvedShaderUniformValue::BundleAsset(_) => {}
                _ => {
                    uniform_handler_fn(
                        &mut (&mut *self, program_handle, reflection),
                        PlumberEvent::SetUniform {
                            name: &uniform.name,
                            value: &uniform.value,
                        },
                    );
                }
            }
        }

        // Now process bundles
        for uniform in uniforms.iter() {
            match uniform.value {
                ResolvedShaderUniformValue::Bundle(ref bundle)
                | ResolvedShaderUniformValue::BundleAsset(ref bundle) => {
                    scope_event!(PlumberEvent::EnterScope);
                    self.plumb(gfx, program_handle, reflection, bundle, uniform_handler_fn);
                    scope_event!(PlumberEvent::LeaveScope);
                }
                _ => {}
            }
        }
    }
}

#[snoozy]
pub async fn compute_tex(
    ctx: Context,
    key: &TextureKey,
    cs: &SnoozyRef<ComputeShader>,
    uniforms: &Vec<ShaderUniformHolder>,
) -> Result<Texture> {
    let cs = ctx.get(cs).await?;
    let uniforms = resolve(ctx, uniforms.clone()).await?;

    unimplemented!()

    /*let mut uniform_plumber = ShaderUniformPlumber::default();

    with_gl(|gl| {
        let output_tex = backend::texture::create_texture(gfx, *key);

        let mut img_unit = {
            unsafe {
                gl.UseProgram(cs.handle);
            }

            uniform_plumber.plumb(
                gl,
                cs.handle,
                &cs.reflection,
                &uniforms,
                &mut |plumber, event| {
                    if let PlumberEvent::SetUniform { value, name } = event {
                        plumber.plumb(gfx, name, value)
                    }
                },
            );
            uniform_plumber.img_unit
        };

        for warning in uniform_plumber.warnings.iter() {
            crate::rtoy_show_warning(format!("{}: {}", cs.name, warning));
        }

        let dispatch_size = (key.width, key.height);

        unsafe {
            let level = 0;
            let layered = gl::FALSE;
            gl.BindImageTexture(
                img_unit as u32,
                output_tex.texture_id,
                level,
                layered,
                0,
                gl::WRITE_ONLY,
                key.format,
            );
            gl.Uniform1i(
                gl.GetUniformLocation(cs.handle, "outputTex\0".as_ptr() as *const i8),
                img_unit,
            );
            gl.Uniform4f(
                gl.GetUniformLocation(cs.handle, "outputTex_size\0".as_ptr() as *const i8),
                dispatch_size.0 as f32,
                dispatch_size.1 as f32,
                1f32 / dispatch_size.0 as f32,
                1f32 / dispatch_size.1 as f32,
            );
            img_unit += 1;

            let mut work_group_size: [i32; 3] = [0, 0, 0];
            gl.GetProgramiv(
                cs.handle,
                gl::COMPUTE_WORK_GROUP_SIZE,
                &mut work_group_size[0],
            );

            gpu_profiler::profile(gfx, &cs.name, || {
                gl.DispatchCompute(
                    (dispatch_size.0 + work_group_size[0] as u32 - 1) / work_group_size[0] as u32,
                    (dispatch_size.1 + work_group_size[1] as u32 - 1) / work_group_size[1] as u32,
                    1,
                )
            });

            for i in 0..img_unit {
                gl.ActiveTexture(gl::TEXTURE0 + i as u32);
                gl.BindTexture(gl::TEXTURE_2D, 0);
            }
        }

        //dbg!(&cs.name);
        gpu_debugger::report_texture(&cs.name, output_tex.texture_id);
        //dbg!(output_tex.texture_id);

        Ok(output_tex)
    })*/
}

#[snoozy]
pub async fn raster_tex(
    ctx: Context,
    key: &TextureKey,
    raster_pipe: &SnoozyRef<RasterPipeline>,
    uniforms: &Vec<ShaderUniformHolder>,
) -> Result<Texture> {
    let uniforms = resolve(ctx.clone(), uniforms.clone()).await?;
    let raster_pipe = ctx.get(raster_pipe).await?;

    unimplemented!()
    /*with_gl(|gl| {
        let output_tex = backend::texture::create_texture(gfx, *key);
        let depth_buffer = create_render_buffer(
            gl,
            RenderBufferKey {
                width: key.width,
                height: key.height,
                format: gl::DEPTH_COMPONENT32F,
            },
        );

        let mut uniform_plumber = ShaderUniformPlumber::default();
        let mut img_unit = 0;

        let fb_handle = {
            let mut handle: u32 = 0;
            unsafe {
                gl.GenFramebuffers(1, &mut handle);
                gl.BindFramebuffer(gl::FRAMEBUFFER, handle);

                gl.FramebufferTexture2D(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    output_tex.texture_id,
                    0,
                );

                gl.FramebufferRenderbuffer(
                    gl::FRAMEBUFFER,
                    gl::DEPTH_ATTACHMENT,
                    gl::RENDERBUFFER,
                    depth_buffer.render_buffer_id,
                );

                gl.BindFramebuffer(gl::FRAMEBUFFER, handle);
            }
            handle
        };

        unsafe {
            gl.UseProgram(raster_pipe.handle);
            gl.Uniform4f(
                gl.GetUniformLocation(raster_pipe.handle, "outputTex_size\0".as_ptr() as *const i8),
                key.width as f32,
                key.height as f32,
                1.0 / key.width as f32,
                1.0 / key.height as f32,
            );
            img_unit += 1;

            gl.Viewport(0, 0, key.width as i32, key.height as i32);
            gl.DepthFunc(gl::GEQUAL);
            gl.Enable(gl::DEPTH_TEST);
            gl.Disable(gl::CULL_FACE);

            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            gl.ClearDepth(0.0);
            gl.Clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT);

            uniform_plumber.img_unit = img_unit;

            #[derive(Default)]
            struct MeshDrawData {
                index_buffer: Option<u32>,
                index_count: Option<u32>,
            }

            let mut mesh_stack = vec![MeshDrawData::default()];

            uniform_plumber.plumb(
                gl,
                raster_pipe.handle,
                &raster_pipe.reflection,
                &uniforms,
                &mut |plumber, event| match event {
                    PlumberEvent::SetUniform { name, value } => {
                        match value {
                            ResolvedShaderUniformValue::BufferAsset(buf)
                                if name == "mesh_index_buf" =>
                            {
                                mesh_stack.last_mut().unwrap().index_buffer = Some(buf.buffer_id);
                            }
                            ResolvedShaderUniformValue::Uint32(value)
                                if name == "mesh_index_count" =>
                            {
                                mesh_stack.last_mut().unwrap().index_count = Some(*value);
                            }
                            _ => {}
                        }

                        plumber.plumb(gfx, name, value)
                    }
                    PlumberEvent::EnterScope => {
                        mesh_stack.push(Default::default());
                    }
                    PlumberEvent::LeaveScope => {
                        let mesh = mesh_stack.pop().unwrap();
                        if let Some(index_count) = mesh.index_count {
                            if let Some(index_buffer) = mesh.index_buffer {
                                gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, index_buffer);
                                gl.DrawElements(
                                    gl::TRIANGLES,
                                    index_count as i32,
                                    gl::UNSIGNED_INT,
                                    std::ptr::null(),
                                );
                                gl.BindBuffer(gl::ELEMENT_ARRAY_BUFFER, 0);
                            } else {
                                gl.DrawArrays(gl::TRIANGLES, 0, index_count as i32);
                            }
                        }
                    }
                },
            );

            gl.BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl.DeleteFramebuffers(1, &fb_handle);

            for i in 0..img_unit {
                gl.ActiveTexture(gl::TEXTURE0 + i as u32);
                gl.BindTexture(gl::TEXTURE_2D, 0);
            }
        }

        Ok(output_tex)
    })*/
}
