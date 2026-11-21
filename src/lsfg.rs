use crate::xbox::Result;
use serde_json::Value;
use std::{env, fs, path::PathBuf, process::Command};

const MANIFEST: &str = "VkLayer_LSFGVK_frame_generation.json";

pub struct Runtime {
    dll: PathBuf,
    layer_dir: PathBuf,
}

impl Runtime {
    pub fn discover() -> Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err("LSFG requires Linux with Vulkan".into());
        }

        let home = env::var_os("HOME").map(PathBuf::from);
        let dll = env::var_os("ALLY_LSFG_DLL").or_else(|| env::var_os("LSFGVK_DLL_PATH"))
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|root| {
                root.join(".local/share/Steam/steamapps/common/Lossless Scaling/lsfg-vk.dll")
            }))
            .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0))
            .ok_or("Install Lossless Scaling from the Steam lsfg-vk branch or set ALLY_LSFG_DLL")?;

        let configured = env::var_os("ALLY_LSFG_LAYER_DIR").map(PathBuf::from);
        let layer_dir = if let Some(path) = configured {
            installed_layer(&path).then_some(path)
        } else {
            let user_share = env::var_os("XDG_DATA_HOME").map(PathBuf::from)
                .or_else(|| home.map(|root| root.join(".local/share")));
            let mut directories: Vec<PathBuf> = env::var_os("VK_ADD_IMPLICIT_LAYER_PATH")
                .map(|paths| env::split_paths(&paths).collect()).unwrap_or_default();
            directories.extend(user_share.into_iter().map(|root| root.join("vulkan/implicit_layer.d")));
            directories.extend(["/usr/local/share/vulkan/implicit_layer.d", "/usr/share/vulkan/implicit_layer.d"]
                .into_iter().map(PathBuf::from));
            directories.into_iter().find(|path| installed_layer(path))
        }.ok_or("Install the lsfg-vk 2.x Vulkan layer or set ALLY_LSFG_LAYER_DIR")?;

        Ok(Self { dll, layer_dir })
    }

    pub fn configure(&self, command: &mut Command) -> Result<()> {
        let mut layers = vec![self.layer_dir.clone()];
        if let Some(paths) = env::var_os("VK_ADD_IMPLICIT_LAYER_PATH") {
            layers.extend(env::split_paths(&paths).filter(|path| path != &self.layer_dir));
        }
        command.env("VK_ADD_IMPLICIT_LAYER_PATH", env::join_paths(layers).map_err(|_| "Invalid Vulkan layer path")?)
            .env_remove("DISABLE_LSFGVK")
            .env_remove("ALLY_VULKAN_CONVERSION")
            .env("DISABLE_LSFG", "1")
            .env("DISABLE_GAMESCOPE_WSI", "1")
            .env("GST_VULKAN_WINDOW", "xcb")
            .env("LSFGVK_ENV", "1")
            .env("LSFGVK_DLL_PATH", &self.dll)
            .env("LSFGVK_MULTIPLIER", "2")
            .env("LSFGVK_PERFORMANCE_MODE", "1")
            .env("LSFGVK_FLOW_SCALE", "1.0")
            .env("LSFGVK_PACING_MODE", "vsync");
        Ok(())
    }
}

fn installed_layer(dir: &std::path::Path) -> bool {
    let Ok(file) = fs::File::open(dir.join(MANIFEST)) else { return false; };
    let Ok(manifest) = serde_json::from_reader::<_, Value>(file) else { return false; };
    let layer = &manifest["layer"];
    layer["name"].as_str() == Some("VK_LAYER_LSFGVK_frame_generation")
        && layer["library_path"].as_str()
            .is_some_and(|library| (!library.contains('/') && !library.is_empty()) || dir.join(library).is_file())
}
