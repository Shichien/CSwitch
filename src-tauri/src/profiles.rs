use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const PROFILE_DIR: &str = "cswitch-profiles";
const LEGACY_PROFILE_DIR: &str = "qpp-profiles";
const PROVIDERS_FILE: &str = "providers.json";
const PROVIDERS_DIR: &str = "providers";
const SETTINGS_FILE: &str = "settings.json";
const CURRENT_FILE: &str = "current";
const GENERATIONS_DIR: &str = "generations";
const EMPTY_GENERATION: &str = "none";
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

pub struct OfficialProfile {
    pub auth: Vec<u8>,
    pub config: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    #[serde(default)]
    pub keep_official_auth: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            keep_official_auth: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRecord {
    pub id: String,
    pub name: String,
    pub api_url: String,
    pub catalog_file: Option<String>,
    pub model_count: usize,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_routing_mode")]
    pub routing_mode: String,
    #[serde(default)]
    pub inference_endpoint: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn default_protocol() -> String {
    "openai_responses".to_string()
}

fn default_routing_mode() -> String {
    "direct".to_string()
}

pub struct ProviderProfile {
    pub record: ProviderRecord,
    pub auth: Vec<u8>,
    pub config: Vec<u8>,
    pub catalog: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderRegistry {
    version: u8,
    providers: Vec<ProviderRecord>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            version: 1,
            providers: Vec::new(),
        }
    }
}

pub struct ProfileStore {
    root: PathBuf,
}

pub(crate) fn migrate_legacy_profile_storage(codex_home: &Path) -> Result<(), Box<dyn Error>> {
    let legacy = codex_home.join(LEGACY_PROFILE_DIR);
    let current = codex_home.join(PROFILE_DIR);
    if !legacy.exists() || current.exists() {
        return Ok(());
    }
    fs::rename(&legacy, &current).map_err(|error| {
        format!(
            "迁移旧供应商目录失败：{} -> {}：{error}",
            legacy.display(),
            current.display()
        )
    })?;
    Ok(())
}

impl ProfileStore {
    pub fn new(codex_home: &Path) -> Self {
        Self {
            root: codex_home.join(PROFILE_DIR),
        }
    }

    pub fn load_official(&self) -> Result<Option<OfficialProfile>, Box<dyn Error>> {
        load_committed_pair(&self.official_dir())
    }

    pub fn load_official_config(&self) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        let directory = self.official_dir();
        if let Some(config) = read_optional(&directory.join("config.toml"))? {
            return Ok(Some(config));
        }
        Ok(load_committed_pair(&directory)?.map(|profile| profile.config))
    }

    pub fn save_official(&self, config: &[u8], auth: &[u8]) -> Result<(), Box<dyn Error>> {
        let directory = self.official_dir();
        create_private_dir(&directory)?;
        ensure_legacy_pair_committed(&directory)?;
        atomic_write_private(&directory.join("config.toml"), config)?;
        commit_pair(&directory, config, auth)
    }

    pub fn save_official_config(&self, config: &[u8]) -> Result<(), Box<dyn Error>> {
        let directory = self.official_dir();
        create_private_dir(&directory)?;
        ensure_legacy_pair_committed(&directory)?;
        atomic_write_private(&directory.join("config.toml"), config)?;
        verify_file(&directory.join("config.toml"), config)
    }

    pub fn discard_official_auth(&self) -> Result<(), Box<dyn Error>> {
        let directory = self.official_dir();
        create_private_dir(&directory)?;
        atomic_write_private(&directory.join(CURRENT_FILE), EMPTY_GENERATION.as_bytes())?;
        let generations = directory.join(GENERATIONS_DIR);
        if generations.is_dir() {
            prune_generations(&generations, &generations.join(EMPTY_GENERATION))?;
        }
        remove_optional_file(&directory.join("auth.json"))
    }

    pub fn provider_registry_exists(&self) -> bool {
        self.registry_path().is_file()
    }

    pub fn ensure_provider_registry(&self) -> Result<(), Box<dyn Error>> {
        if !self.provider_registry_exists() {
            self.save_registry(&ProviderRegistry::default())?;
        }
        Ok(())
    }

    pub fn load_settings(&self) -> Result<AppSettings, Box<dyn Error>> {
        let Some(content) = read_optional(&self.settings_path())? else {
            return Ok(AppSettings::default());
        };
        serde_json::from_slice(&content)
            .map_err(|error| format!("应用设置格式无效：{error}").into())
    }

    pub fn save_settings(&self, settings: &AppSettings) -> Result<(), Box<dyn Error>> {
        create_private_dir(&self.root)?;
        let content = serde_json::to_vec_pretty(settings)?;
        atomic_write_private(&self.settings_path(), &content)?;
        verify_file(&self.settings_path(), &content)
    }

    pub fn keep_official_auth(&self) -> Result<bool, Box<dyn Error>> {
        Ok(self.load_settings()?.keep_official_auth)
    }

    pub fn list_providers(&self) -> Result<Vec<ProviderRecord>, Box<dyn Error>> {
        Ok(self.load_registry()?.providers)
    }

    pub fn load_provider(&self, id: &str) -> Result<ProviderProfile, Box<dyn Error>> {
        validate_provider_id(id)?;
        let registry = self.load_registry()?;
        let record = registry
            .providers
            .into_iter()
            .find(|provider| provider.id == id)
            .ok_or_else(|| format!("供应商不存在：{id}"))?;
        let directory = self.provider_dir(id);
        let auth = fs::read(directory.join("auth.json"))
            .map_err(|error| format!("读取供应商 auth.json 失败：{error}"))?;
        let config = fs::read(directory.join("config.toml"))
            .map_err(|error| format!("读取供应商 config.toml 失败：{error}"))?;
        let catalog = match record.catalog_file.as_deref() {
            Some(file) => {
                validate_catalog_file(file)?;
                let path = directory.join(file);
                let content = fs::read(&path)
                    .map_err(|error| format!("读取供应商 models.json 失败：{error}"))?;
                Some(content)
            }
            None => None,
        };
        Ok(ProviderProfile {
            record,
            auth,
            config,
            catalog,
        })
    }

    pub fn save_provider(
        &self,
        id: Option<&str>,
        name: &str,
        api_url: &str,
        auth: &[u8],
        catalog: Option<&[u8]>,
        config: &[u8],
    ) -> Result<ProviderRecord, Box<dyn Error>> {
        let inference_endpoint = format!("{}/responses", api_url.trim_end_matches('/'));
        self.save_provider_with_routing(
            id,
            name,
            api_url,
            auth,
            catalog,
            "openai_responses",
            "direct",
            Some(&inference_endpoint),
            config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn save_provider_with_routing(
        &self,
        id: Option<&str>,
        name: &str,
        api_url: &str,
        auth: &[u8],
        catalog: Option<&[u8]>,
        protocol: &str,
        routing_mode: &str,
        inference_endpoint: Option<&str>,
        config: &[u8],
    ) -> Result<ProviderRecord, Box<dyn Error>> {
        validate_routing(protocol, routing_mode, inference_endpoint)?;
        let mut registry = self.load_registry()?;
        let existing_index = match id {
            Some(id) => {
                validate_provider_id(id)?;
                Some(
                    registry
                        .providers
                        .iter()
                        .position(|provider| provider.id == id)
                        .ok_or_else(|| format!("供应商不存在：{id}"))?,
                )
            }
            None => None,
        };
        if registry
            .providers
            .iter()
            .enumerate()
            .any(|(index, provider)| {
                Some(index) != existing_index && provider.name.eq_ignore_ascii_case(name)
            })
        {
            return Err(format!("供应商名称已存在：{name}").into());
        }

        let id = existing_index
            .map(|index| registry.providers[index].id.clone())
            .unwrap_or_else(generate_provider_id);
        let directory = self.provider_dir(&id);
        create_private_dir(&directory)?;
        let catalog_path = catalog.map(|_| directory.join(generate_catalog_file()));
        let auth_path = directory.join("auth.json");
        let config_path = directory.join("config.toml");
        let previous_auth = read_optional(&auth_path)?;
        let previous_config = read_optional(&config_path)?;

        let result = (|| -> Result<ProviderRecord, Box<dyn Error>> {
            if let (Some(path), Some(content)) = (catalog_path.as_deref(), catalog) {
                atomic_write_private(path, content)?;
                verify_file(path, content)?;
            }
            atomic_write_private(&auth_path, auth)?;
            atomic_write_private(&config_path, config)?;
            verify_file(&auth_path, auth)?;
            verify_file(&config_path, config)?;

            let now = Utc::now().to_rfc3339();
            let record = ProviderRecord {
                id: id.clone(),
                name: name.to_string(),
                api_url: api_url.to_string(),
                catalog_file: catalog_path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|file| file.to_str())
                    .map(str::to_string)
                    .or_else(|| {
                        existing_index
                            .and_then(|index| registry.providers[index].catalog_file.clone())
                    }),
                model_count: match catalog {
                    Some(content) => count_catalog_models(content)?,
                    None => existing_index
                        .map(|index| registry.providers[index].model_count)
                        .unwrap_or(0),
                },
                protocol: protocol.to_string(),
                routing_mode: routing_mode.to_string(),
                inference_endpoint: inference_endpoint.map(str::to_string),
                created_at: existing_index
                    .map(|index| registry.providers[index].created_at.clone())
                    .unwrap_or_else(|| now.clone()),
                updated_at: now,
            };
            if let Some(index) = existing_index {
                registry.providers[index] = record.clone();
            } else {
                registry.providers.push(record.clone());
            }
            self.save_registry(&registry)?;
            Ok(record)
        })();

        if result.is_err() {
            let _ = restore_optional_file(&auth_path, previous_auth.as_deref());
            let _ = restore_optional_file(&config_path, previous_config.as_deref());
            if let Some(path) = catalog_path.as_deref() {
                let _ = remove_optional_file(path);
            }
        }
        result
    }

    pub fn save_provider_without_catalog(
        &self,
        name: &str,
        api_url: &str,
        auth: &[u8],
        config: &[u8],
    ) -> Result<ProviderRecord, Box<dyn Error>> {
        self.save_provider(None, name, api_url, auth, None, config)
    }

    pub fn update_provider_snapshot(
        &self,
        id: &str,
        config: &[u8],
        auth: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        validate_provider_id(id)?;
        if !self
            .load_registry()?
            .providers
            .iter()
            .any(|provider| provider.id == id)
        {
            return Err(format!("供应商不存在：{id}").into());
        }
        let directory = self.provider_dir(id);
        atomic_write_private(&directory.join("config.toml"), config)?;
        atomic_write_private(&directory.join("auth.json"), auth)?;
        verify_file(&directory.join("config.toml"), config)?;
        verify_file(&directory.join("auth.json"), auth)
    }

    pub fn update_provider_routing(
        &self,
        id: &str,
        protocol: &str,
        routing_mode: &str,
        inference_endpoint: Option<&str>,
    ) -> Result<ProviderRecord, Box<dyn Error>> {
        validate_provider_id(id)?;
        validate_routing(protocol, routing_mode, inference_endpoint)?;
        let mut registry = self.load_registry()?;
        let provider = registry
            .providers
            .iter_mut()
            .find(|provider| provider.id == id)
            .ok_or_else(|| format!("供应商不存在：{id}"))?;
        provider.protocol = protocol.to_string();
        provider.routing_mode = routing_mode.to_string();
        provider.inference_endpoint = inference_endpoint.map(str::to_string);
        provider.updated_at = Utc::now().to_rfc3339();
        let updated = provider.clone();
        self.save_registry(&registry)?;
        Ok(updated)
    }

    pub fn delete_provider(&self, id: &str) -> Result<(), Box<dyn Error>> {
        validate_provider_id(id)?;
        let mut registry = self.load_registry()?;
        let index = registry
            .providers
            .iter()
            .position(|provider| provider.id == id)
            .ok_or_else(|| format!("供应商不存在：{id}"))?;
        registry.providers.remove(index);
        self.save_registry(&registry)?;
        let directory = self.provider_dir(id);
        if directory.is_dir() {
            fs::remove_dir_all(&directory)
                .map_err(|error| format!("删除供应商文件失败 {}：{error}", directory.display()))?;
            sync_directory(directory.parent().ok_or("供应商目录没有父目录")?)?;
        }
        Ok(())
    }

    pub fn load_custom_config(&self) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        let directory = self.custom_dir();
        if let Some(profile) = load_committed_pair(&directory)? {
            return Ok(Some(profile.config));
        }
        read_optional(&directory.join("config.toml"))
    }

    pub fn load_custom_auth(&self) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        let directory = self.custom_dir();
        if let Some(profile) = load_committed_pair(&directory)? {
            return Ok(Some(profile.auth));
        }
        read_optional(&directory.join("auth.json"))
    }

    pub fn save_custom_config(&self, config: &[u8]) -> Result<(), Box<dyn Error>> {
        let directory = self.custom_dir();
        create_private_dir(&directory)?;
        ensure_legacy_pair_committed(&directory)?;
        atomic_write_private(&directory.join("config.toml"), config)?;
        verify_file(&directory.join("config.toml"), config)
    }

    pub fn save_custom_auth(&self, auth: &[u8]) -> Result<(), Box<dyn Error>> {
        let directory = self.custom_dir();
        create_private_dir(&directory)?;
        ensure_legacy_pair_committed(&directory)?;
        atomic_write_private(&directory.join("auth.json"), auth)?;
        verify_file(&directory.join("auth.json"), auth)?;
        if let Some(config) = read_optional(&directory.join("config.toml"))? {
            commit_pair(&directory, &config, auth)?;
        }
        Ok(())
    }

    fn official_dir(&self) -> PathBuf {
        self.root.join("official")
    }

    fn custom_dir(&self) -> PathBuf {
        self.root.join("custom")
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join(PROVIDERS_FILE)
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    fn provider_dir(&self, id: &str) -> PathBuf {
        self.root.join(PROVIDERS_DIR).join(id)
    }

    fn load_registry(&self) -> Result<ProviderRegistry, Box<dyn Error>> {
        let Some(content) = read_optional(&self.registry_path())? else {
            return Ok(ProviderRegistry::default());
        };
        let registry: ProviderRegistry = serde_json::from_slice(&content)
            .map_err(|error| format!("供应商列表格式无效：{error}"))?;
        if registry.version != 1 {
            return Err(format!("不支持的供应商列表版本：{}", registry.version).into());
        }
        for provider in &registry.providers {
            validate_provider_id(&provider.id)?;
            if let Some(file) = provider.catalog_file.as_deref() {
                validate_catalog_file(file)?;
            }
        }
        Ok(registry)
    }

    fn save_registry(&self, registry: &ProviderRegistry) -> Result<(), Box<dyn Error>> {
        let content = serde_json::to_vec_pretty(registry)?;
        atomic_write_private(&self.registry_path(), &content)?;
        verify_file(&self.registry_path(), &content)
    }
}

fn validate_routing(
    protocol: &str,
    routing_mode: &str,
    inference_endpoint: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if !matches!(
        protocol,
        "openai_responses" | "openai_chat" | "anthropic_messages"
    ) {
        return Err(format!("不支持的上游协议：{protocol}").into());
    }
    if !matches!(routing_mode, "direct" | "local") {
        return Err(format!("不支持的路由模式：{routing_mode}").into());
    }
    if protocol == "openai_responses" && routing_mode != "direct" {
        return Err("Responses 供应商必须使用直连模式".into());
    }
    if protocol != "openai_responses" && inference_endpoint.is_none() {
        return Err("需要协议转换的供应商缺少推理接口地址".into());
    }
    Ok(())
}

fn validate_provider_id(id: &str) -> Result<(), Box<dyn Error>> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("供应商 ID 无效".into());
    }
    Ok(())
}

fn validate_catalog_file(file: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(file);
    if path.components().count() != 1 || !file.starts_with("models-") || !file.ends_with(".json") {
        return Err("供应商模型目录文件名无效".into());
    }
    Ok(())
}

fn generate_provider_id() -> String {
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("provider-{created_at:x}-{sequence:x}")
}

fn generate_catalog_file() -> String {
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("models-{created_at:x}-{sequence:x}.json")
}

fn count_catalog_models(content: &[u8]) -> Result<usize, Box<dyn Error>> {
    let document: serde_json::Value = serde_json::from_slice(content)?;
    document
        .get("models")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| "models.json 缺少 models 数组".into())
}

fn restore_optional_file(path: &Path, content: Option<&[u8]>) -> Result<(), Box<dyn Error>> {
    match content {
        Some(content) => atomic_write_private(path, content),
        None => remove_optional_file(path),
    }
}

fn load_committed_pair(directory: &Path) -> Result<Option<OfficialProfile>, Box<dyn Error>> {
    let current = read_optional(&directory.join(CURRENT_FILE))?;
    if let Some(current) = current {
        let generation = std::str::from_utf8(&current)
            .map_err(|error| format!("快照指针不是 UTF-8：{error}"))?
            .trim();
        if generation == EMPTY_GENERATION {
            return Ok(None);
        }
        validate_generation(generation)?;
        let root = directory.join(GENERATIONS_DIR).join(generation);
        let config = fs::read(root.join("config.toml"))
            .map_err(|error| format!("已提交快照缺少 config.toml（{generation}）：{error}"))?;
        let auth = fs::read(root.join("auth.json"))
            .map_err(|error| format!("已提交快照缺少 auth.json（{generation}）：{error}"))?;
        return Ok(Some(OfficialProfile { auth, config }));
    }

    let auth = read_optional(&directory.join("auth.json"))?;
    let config = read_optional(&directory.join("config.toml"))?;
    Ok(match (auth, config) {
        (Some(auth), Some(config)) => Some(OfficialProfile { auth, config }),
        _ => None,
    })
}

fn ensure_legacy_pair_committed(directory: &Path) -> Result<(), Box<dyn Error>> {
    if directory.join(CURRENT_FILE).is_file() {
        return Ok(());
    }
    let config = read_optional(&directory.join("config.toml"))?;
    let auth = read_optional(&directory.join("auth.json"))?;
    if let (Some(config), Some(auth)) = (config, auth) {
        commit_pair(directory, &config, &auth)?;
    }
    Ok(())
}

fn commit_pair(directory: &Path, config: &[u8], auth: &[u8]) -> Result<(), Box<dyn Error>> {
    create_private_dir(directory)?;
    let generations = directory.join(GENERATIONS_DIR);
    create_private_dir(&generations)?;
    let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let created_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let generation = format!("generation-{}-{created_at}-{sequence}", std::process::id());
    let staging = generations.join(format!(".{generation}.tmp"));
    let destination = generations.join(&generation);
    let result = (|| -> Result<(), Box<dyn Error>> {
        fs::create_dir(&staging)?;
        #[cfg(unix)]
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
        write_new_private(&staging.join("config.toml"), config)?;
        write_new_private(&staging.join("auth.json"), auth)?;
        verify_file(&staging.join("config.toml"), config)?;
        verify_file(&staging.join("auth.json"), auth)?;
        sync_directory(&staging)?;
        fs::rename(&staging, &destination)?;
        sync_directory(&generations)?;
        atomic_write_private(&directory.join(CURRENT_FILE), generation.as_bytes())?;
        prune_generations(&generations, &destination)?;
        remove_optional_file(&directory.join("auth.json"))?;
        Ok(())
    })();
    if result.is_err() && staging.is_dir() {
        let _ = fs::remove_dir_all(staging);
    }
    result
}

fn prune_generations(generations: &Path, current: &Path) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(generations)? {
        let entry = entry?;
        let path = entry.path();
        if path == current {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    sync_directory(generations)
}

fn validate_generation(generation: &str) -> Result<(), Box<dyn Error>> {
    if generation.is_empty()
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("快照指针中的代次名称无效".into());
    }
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
    match fs::read(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("读取快照文件失败 {}：{error}", path.display()).into()),
    }
}

fn create_private_dir(path: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(path).map_err(|error| -> Box<dyn Error> {
        format!("创建快照目录失败 {}：{error}", path.display()).into()
    })?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(
        |error| -> Box<dyn Error> {
            format!("设置快照目录权限失败 {}：{error}", path.display()).into()
        },
    )?;
    Ok(())
}

fn remove_optional_file(path: &Path) -> Result<(), Box<dyn Error>> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(path.parent().ok_or("快照文件没有父目录")?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_new_private(path: &Path, content: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(content)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<(), Box<dyn Error>> {
    let parent = path.parent().ok_or("快照文件没有父目录")?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!(
        ".{}.cswitch-{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("profile"),
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), Box<dyn Error>> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("写入快照文件失败 {}：{error}", path.display()).into())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), Box<dyn Error>> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

fn verify_file(path: &Path, expected: &[u8]) -> Result<(), Box<dyn Error>> {
    if fs::read(path)? != expected {
        return Err(format!("快照写入验证失败：{}", path.display()).into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stores_official_pair_and_custom_config_separately() {
        let directory = tempdir().expect("tempdir");
        let store = ProfileStore::new(directory.path());
        store
            .save_official(b"model = \"official\"\n", b"{\"tokens\":{}}")
            .expect("save official");
        store
            .save_custom_config(b"model_provider = \"custom\"\n")
            .expect("save custom");

        let official = store
            .load_official()
            .expect("load official")
            .expect("official profile");
        assert_eq!(official.config, b"model = \"official\"\n");
        assert_eq!(official.auth, b"{\"tokens\":{}}");
        assert_eq!(
            store
                .load_custom_config()
                .expect("load custom")
                .expect("custom config"),
            b"model_provider = \"custom\"\n"
        );
        store
            .save_custom_auth(b"{\"OPENAI_API_KEY\":\"fixture-key\"}")
            .expect("save custom auth");
        assert_eq!(
            store
                .load_custom_auth()
                .expect("load custom auth")
                .expect("custom auth"),
            b"{\"OPENAI_API_KEY\":\"fixture-key\"}"
        );
    }

    #[test]
    fn custom_profile_never_exposes_a_half_written_pair() {
        let directory = tempdir().expect("tempdir");
        let store = ProfileStore::new(directory.path());
        store
            .save_custom_config(b"base_url = \"https://first.example\"\n")
            .expect("save first config");
        store
            .save_custom_auth(b"{\"OPENAI_API_KEY\":\"first-key\"}")
            .expect("commit first pair");

        store
            .save_custom_config(b"base_url = \"https://second.example\"\n")
            .expect("stage second config");

        assert_eq!(
            store
                .load_custom_config()
                .expect("load committed config")
                .expect("committed config"),
            b"base_url = \"https://first.example\"\n"
        );
        assert_eq!(
            store
                .load_custom_auth()
                .expect("load committed auth")
                .expect("committed auth"),
            b"{\"OPENAI_API_KEY\":\"first-key\"}"
        );

        store
            .save_custom_auth(b"{\"OPENAI_API_KEY\":\"second-key\"}")
            .expect("commit second pair");
        assert_eq!(
            store
                .load_custom_config()
                .expect("load second config")
                .expect("second config"),
            b"base_url = \"https://second.example\"\n"
        );
        assert_eq!(
            store
                .load_custom_auth()
                .expect("load second auth")
                .expect("second auth"),
            b"{\"OPENAI_API_KEY\":\"second-key\"}"
        );
        let generations = directory
            .path()
            .join(PROFILE_DIR)
            .join("custom")
            .join(GENERATIONS_DIR);
        let entries = fs::read_dir(generations)
            .expect("read generations")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect generations");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].file_type().expect("generation type").is_dir());
        assert!(
            !directory
                .path()
                .join(PROFILE_DIR)
                .join("custom/auth.json")
                .exists()
        );
    }

    #[test]
    fn discarding_official_auth_invalidates_the_committed_pair() {
        let directory = tempdir().expect("tempdir");
        let store = ProfileStore::new(directory.path());
        store
            .save_official(b"model = \"official\"\n", b"{\"tokens\":{}}")
            .expect("save official pair");

        store
            .discard_official_auth()
            .expect("discard official auth");

        assert!(store.load_official().expect("load official").is_none());
        assert_eq!(
            store
                .load_official_config()
                .expect("load retained config")
                .expect("retained config"),
            b"model = \"official\"\n"
        );
        assert_eq!(
            fs::read_dir(
                directory
                    .path()
                    .join(PROFILE_DIR)
                    .join("official")
                    .join(GENERATIONS_DIR)
            )
            .expect("read discarded generations")
            .count(),
            0
        );
    }

    #[test]
    fn migrates_legacy_profiles_once() {
        let directory = tempdir().expect("tempdir");
        let legacy = directory.path().join(LEGACY_PROFILE_DIR);
        fs::create_dir_all(&legacy).expect("create legacy profile directory");
        fs::write(legacy.join("marker"), b"legacy").expect("write legacy marker");

        migrate_legacy_profile_storage(directory.path()).expect("migrate legacy profiles");
        migrate_legacy_profile_storage(directory.path()).expect("repeat migration");

        assert!(!legacy.exists());
        assert_eq!(
            fs::read(directory.path().join(PROFILE_DIR).join("marker"))
                .expect("read migrated marker"),
            b"legacy"
        );
    }

    #[test]
    fn existing_cswitch_profiles_are_never_overwritten_by_legacy_profiles() {
        let directory = tempdir().expect("tempdir");
        let legacy = directory.path().join(LEGACY_PROFILE_DIR);
        let current = directory.path().join(PROFILE_DIR);
        fs::create_dir_all(&legacy).expect("create legacy profile directory");
        fs::create_dir_all(&current).expect("create current profile directory");
        fs::write(legacy.join("marker"), b"legacy").expect("write legacy marker");
        fs::write(current.join("marker"), b"current").expect("write current marker");

        migrate_legacy_profile_storage(directory.path()).expect("skip conflicting migration");

        assert_eq!(
            fs::read(legacy.join("marker")).expect("read legacy"),
            b"legacy"
        );
        assert_eq!(
            fs::read(current.join("marker")).expect("read current"),
            b"current"
        );
    }

    #[test]
    fn keep_official_auth_defaults_off_and_round_trips() {
        let directory = tempdir().expect("tempdir");
        let store = ProfileStore::new(directory.path());
        assert!(!store.keep_official_auth().expect("default setting"));
        store
            .save_settings(&AppSettings {
                keep_official_auth: true,
            })
            .expect("save settings");
        assert!(store.keep_official_auth().expect("saved setting"));
    }
}
