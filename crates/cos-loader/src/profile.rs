//! cordis.yml 条目解析（语义参考：`vendor/loader/src/config/entry.ts` 的 `EntryOptions`）。

use std::path::Path;

use serde::Deserialize;

use crate::error::LoadError;

/// 一个已配置的插件节点（v1：无 bundle/patch 层叠，cordis.yml 即单一清单）。
#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    /// 稳定实例 id（缺省 = `name`）。
    #[serde(default)]
    pub id: Option<String>,
    /// 工厂名（[`crate::resolve_factory`] 的键）。
    pub name: String,
    /// 插件配置（YAML 原位解析为 JSON；缺省 `null`）。
    #[serde(default)]
    pub config: serde_json::Value,
    /// 额外依赖的服务名（叠加在插件自身 `inject()` 之上）。
    #[serde(default)]
    pub inject: Vec<String>,
    /// 禁用：跳过装载（同 dsh 的 disabled 字段）。
    #[serde(default)]
    pub disabled: bool,
}

impl Entry {
    /// 有效实例 id（缺省 = 工厂名）。
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.name)
    }
}

/// cordis.yml：顶层 YAML 数组（v1 即完整清单）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile(pub Vec<Entry>);

impl Profile {
    /// 解析 YAML 文本。
    pub fn parse(yaml: &str) -> Result<Self, LoadError> {
        Ok(serde_yaml::from_str(yaml)?)
    }

    /// 读取并解析 YAML 文件。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }
}
