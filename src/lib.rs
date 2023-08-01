use anyhow::{Context, Result};
use serde::Deserialize;
use std::env;
use std::fs;
use std::time::SystemTime;
use virt_env::{create_env_virt, strip_env_virt};
use virt_io::{
    create_io_virt, strip_clocks_virt, strip_fs_virt, strip_http_virt, strip_io_virt,
    strip_sockets_virt, strip_stdio_virt, stub_clocks_virt, stub_http_virt, stub_sockets_virt,
    VirtStdio,
};
use walrus::ValType;
use walrus_ops::add_stub_exported_func;
use wasm_metadata::Producers;
use wasm_opt::{Feature, OptimizationOptions};
use wit_component::{metadata, ComponentEncoder, StringEncoding};

mod data;
mod virt_env;
mod virt_io;
mod walrus_ops;

pub use virt_env::{HostEnv, VirtEnv};
pub use virt_io::{FsEntry, VirtFs, VirtualFiles};

/// Virtualization options
///
/// When subsystems are not virtualized, their imports and exports
/// are simply ignored by the virtualizer, allowing for the creation
/// of subsystem-specific virtualizations.
///
/// Note: The default virtualization for all virtualization modes is
/// full encapsulation.
///
#[derive(Deserialize, Debug, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WasiVirt {
    /// Environment virtualization
    pub env: Option<VirtEnv>,
    /// Filesystem virtualization
    pub fs: Option<VirtFs>,
    /// Stdio virtualization
    pub stdio: Option<VirtStdio>,
    /// Exit virtualization
    pub exit: Option<bool>,
    /// Clocks virtualization
    pub clocks: Option<bool>,
    /// Http virtualization
    pub http: Option<bool>,
    /// Sockets virtualization
    pub sockets: Option<bool>,
    /// Random virtualization
    pub random: Option<bool>,
    /// Disable wasm-opt run if desired
    pub wasm_opt: Option<bool>,
}

pub struct VirtResult {
    pub adapter: Vec<u8>,
    pub virtual_files: VirtualFiles,
}

impl WasiVirt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all(&mut self, allow: bool) {
        self.clocks(allow);
        self.http(allow);
        self.sockets(allow);
        self.exit(allow);
        if allow {
            self.env().allow_all();
        } else {
            self.env().deny_all();
        }
        if allow {
            self.fs().allow_host_preopens();
        } else {
            self.fs().deny_host_preopens();
        }
        self.stdio().all(allow);
    }

    pub fn clocks(&mut self, allow: bool) {
        self.clocks = Some(allow);
    }

    pub fn http(&mut self, allow: bool) {
        self.http = Some(allow);
    }

    pub fn sockets(&mut self, allow: bool) {
        self.sockets = Some(allow);
    }

    pub fn exit(&mut self, allow: bool) {
        self.exit = Some(allow);
    }

    pub fn random(&mut self, allow: bool) {
        self.random = Some(allow);
    }

    pub fn env(&mut self) -> &mut VirtEnv {
        self.env.get_or_insert_with(Default::default)
    }

    pub fn fs(&mut self) -> &mut VirtFs {
        self.fs.get_or_insert_with(Default::default)
    }

    pub fn stdio(&mut self) -> &mut VirtStdio {
        self.stdio.get_or_insert_with(Default::default)
    }

    pub fn opt(&mut self, opt: bool) {
        self.wasm_opt = Some(opt);
    }

    pub fn finish(&mut self) -> Result<VirtResult> {
        let virt_adapter = include_bytes!("../lib/virtual_adapter.wasm");

        let config = walrus::ModuleConfig::new();
        let mut module = config.parse(virt_adapter)?;
        module.name = Some("wasi_virt".into());

        // only env virtualization is independent of io
        if let Some(env) = &self.env {
            create_env_virt(&mut module, env)?;
        }

        let has_io = self.fs.is_some()
            || self.stdio.is_some()
            || self.clocks.is_some()
            || self.http.is_some()
            || self.sockets.is_some();

        let virtual_files = if has_io {
            // io virt is managed through a singular io configuration
            create_io_virt(&mut module, self.fs.as_ref(), self.stdio.as_ref())?
        } else {
            Default::default()
        };

        // decode the component custom section to strip out the unused world exports
        // before reencoding.
        let mut component_section = module
            .customs
            .remove_raw("component-type:virtual-adapter")
            .context("Unable to find component section")?;

        let (_, mut bindgen) = metadata::decode(virt_adapter)?;
        let (_, pkg_id) = bindgen
            .resolve
            .package_names
            .iter()
            .find(|(name, _)| name.namespace == "local")
            .unwrap();

        let base_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-base"))?;

        let env_world = bindgen.resolve.select_world(*pkg_id, Some("virtual-env"))?;

        let io_world = bindgen.resolve.select_world(*pkg_id, Some("virtual-io"))?;
        let io_clocks_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-io-clocks"))?;
        let io_http_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-io-http"))?;
        let io_sockets_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-io-sockets"))?;

        let exit_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-exit"))?;
        let fs_world = bindgen.resolve.select_world(*pkg_id, Some("virtual-fs"))?;
        let random_world = bindgen.resolve.select_world(*pkg_id, Some("virtual-fs"))?;
        let stdio_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-stdio"))?;
        let clocks_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-clocks"))?;
        let http_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-http"))?;
        let sockets_world = bindgen
            .resolve
            .select_world(*pkg_id, Some("virtual-sockets"))?;

        // env & exit subsystems are fully independent
        if self.env.is_some() {
            bindgen.resolve.merge_worlds(env_world, base_world)?;
        } else {
            strip_env_virt(&mut module)?;
        }
        if let Some(exit) = self.exit {
            if !exit {
                bindgen.resolve.merge_worlds(exit_world, base_world)?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:cli-base/exit#exit",
                    vec![ValType::I32],
                    vec![],
                )?;
            }
        }
        if let Some(random) = self.random {
            if !random {
                bindgen.resolve.merge_worlds(random_world, base_world)?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:random/random#get-random-bytes",
                    vec![ValType::I64],
                    vec![ValType::I32, ValType::I32],
                )?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:random/random#get-random-u64",
                    vec![],
                    vec![ValType::I64],
                )?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:random/insecure#get-insecure-random-bytes",
                    vec![ValType::I64],
                    vec![ValType::I32, ValType::I32],
                )?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:random/insecure#get-insecure-random-u64",
                    vec![],
                    vec![ValType::I64],
                )?;
                add_stub_exported_func(
                    &mut module,
                    "wasi:random/insecure-seed#insecure-seed",
                    vec![ValType::I64],
                    vec![ValType::I32],
                )?;
            }
        }

        // io subsystems have io dependence due to streams + poll
        // therefore we need to strip just their io dependence portion
        if has_io {
            bindgen.resolve.merge_worlds(io_world, base_world)?;
        } else {
            strip_io_virt(&mut module)?;
        }
        if let Some(clocks) = self.clocks {
            if clocks {
                // When subsystem is enabled, we can pass through all interfaces
                // that do not rely on io. The adapter default is passthrough.
                bindgen.resolve.merge_worlds(io_clocks_world, base_world)?;
            } else {
                // When subsystem is disabled, we must do a full virtualization
                bindgen.resolve.merge_worlds(clocks_world, base_world)?;
                stub_clocks_virt(&mut module)?;
            }
        } else {
            strip_clocks_virt(&mut module)?;
        }
        // sockets and http are identical to clocks above
        if let Some(sockets) = self.sockets {
            if sockets {
                bindgen.resolve.merge_worlds(io_sockets_world, base_world)?;
            } else {
                bindgen.resolve.merge_worlds(sockets_world, base_world)?;
                stub_sockets_virt(&mut module)?;
            }
        } else {
            strip_sockets_virt(&mut module)?;
        }
        if let Some(http) = self.http {
            if http {
                bindgen.resolve.merge_worlds(io_http_world, base_world)?;
            } else {
                bindgen.resolve.merge_worlds(http_world, base_world)?;
                stub_http_virt(&mut module)?;
            }
        } else {
            strip_http_virt(&mut module)?;
        }

        // stdio and fs are fully implemented in io world
        // (all their interfaces use streams)
        if self.stdio.is_some() {
            bindgen.resolve.merge_worlds(stdio_world, base_world)?;
        } else {
            strip_stdio_virt(&mut module)?;
        }
        if self.fs.is_some() {
            bindgen.resolve.merge_worlds(fs_world, base_world)?;
        } else {
            strip_fs_virt(&mut module)?;
        }

        let mut producers = Producers::default();
        producers.add("processed-by", "wasi-virt", env!("CARGO_PKG_VERSION"));

        component_section.data = metadata::encode(
            &bindgen.resolve,
            base_world,
            StringEncoding::UTF8,
            Some(&producers),
        )?;

        module.customs.add(component_section);

        let mut bytes = module.emit_wasm();

        // because we rely on dead code ellimination to remove unnecessary adapter code
        // we save into a temporary file and run wasm-opt before returning
        // this can be disabled with wasm_opt: false
        if self.wasm_opt.unwrap_or(true) {
            let dir = env::temp_dir();
            let tmp_input = dir.join(format!("virt.core.input.{}.wasm", timestamp()));
            let tmp_output = dir.join(format!("virt.core.output.{}.wasm", timestamp()));
            fs::write(&tmp_input, bytes)
                .with_context(|| "Unable to write temporary file for wasm-opt call on adapter")?;
            OptimizationOptions::new_optimize_for_size_aggressively()
                .enable_feature(Feature::ReferenceTypes)
                .run(&tmp_input, &tmp_output)
                .with_context(|| "Unable to apply wasm-opt optimization to virt. This can be disabled with wasm_opt: false.")
                .or_else(|e| {
                    fs::remove_file(&tmp_input)?;
                    Err(e)
                })?;
            bytes = fs::read(&tmp_output)?;
            fs::remove_file(&tmp_input)?;
            fs::remove_file(&tmp_output)?;
        }

        // now adapt the virtualized component
        let encoder = ComponentEncoder::default().validate(true).module(&bytes)?;
        let encoded = encoder.encode()?;

        Ok(VirtResult {
            adapter: encoded,
            virtual_files,
        })
    }
}

fn timestamp() -> u64 {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(n) => n.as_secs(),
        Err(_) => panic!(),
    }
}