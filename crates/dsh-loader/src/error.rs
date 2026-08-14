//! 装载边界错误（决策 D5）：带插件/条目上下文的可读报错（同 JS `failed to apply loader entry`）。

use dsh_core::CoreError;
use thiserror::Error;

/// dsh-loader 的边界错误。
#[derive(Debug, Error)]
pub enum LoadError {
    /// 读取配置失败。
    #[error("读取配置失败: {0}")]
    Io(#[from] std::io::Error),
    /// 解析 cordis.yml 失败。
    #[error("解析 cordis.yml 失败: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// 插件工厂未注册（静态注册表里没有这个名字）。
    #[error("插件 '{name}' 未注册（可用: {available:?}）")]
    UnknownPlugin {
        /// 配置里的工厂名。
        name: String,
        /// 已注册的工厂名（便于排查拼写）。
        available: Vec<&'static str>,
    },
    /// 条目配置反序列化失败。
    #[error("failed to apply loader entry {id} ({name}): 配置解析失败: {source}")]
    ConfigParse {
        /// 条目实例 id。
        id: String,
        /// 工厂名。
        name: String,
        /// serde 反序列化错误。
        #[source]
        source: serde_json::Error,
    },
    /// 条目配置校验失败（`Validate::validate`）。
    #[error("failed to apply loader entry {id} ({name}): 配置校验失败: {source}")]
    ConfigInvalid {
        /// 条目实例 id。
        id: String,
        /// 工厂名。
        name: String,
        /// 校验错误。
        #[source]
        source: CoreError,
    },
    /// 依赖服务无提供者。
    #[error("服务 '{service}' 无提供者（'{plugin}' 需要）")]
    MissingDependency {
        /// 需要该服务的插件条目。
        plugin: String,
        /// 缺失的服务名。
        service: String,
    },
    /// 同一服务被多个插件提供。
    #[error("服务 '{service}' 被重复提供: {plugins:?}")]
    DuplicateProvide {
        /// 冲突的服务名。
        service: String,
        /// 提供者条目 id 列表。
        plugins: Vec<String>,
    },
    /// 依赖环（无法拓扑排序的插件条目）。
    #[error("依赖环: {cycle:?}（无法确定装载顺序）")]
    DependencyCycle {
        /// 环上的插件条目 id。
        cycle: Vec<String>,
    },
    /// 插件 apply 失败。
    #[error("failed to apply loader entry {id} ({name}): {source}")]
    Apply {
        /// 条目实例 id。
        id: String,
        /// 工厂名。
        name: String,
        /// apply 返回的错误。
        #[source]
        source: CoreError,
    },
    /// 其他失败。
    #[error("{0}")]
    Other(String),
}
