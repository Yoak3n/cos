//! P0 验收：hello 插件 apply 并在 Context 上 provide 一个服务、get 取回。

use std::sync::Arc;

use dsh_core::{Context, CoreError, Plugin, Service, Validate};
use serde::Deserialize;

struct Greeting {
    message: String,
}

impl Service for Greeting {
    const NAME: &'static str = "greeting";
}

#[derive(Deserialize, Default)]
struct HelloConfig {
    #[serde(default)]
    message: String,
}

impl Validate for HelloConfig {}

struct HelloPlugin;

impl Plugin for HelloPlugin {
    const ID: &'static str = "plugin-hello";

    type Config = HelloConfig;

    fn provide(&self) -> &'static [&'static str] {
        &["greeting"]
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        ctx.provide(Greeting {
            message: config.message.clone(),
        })?;
        Ok(())
    }
}

#[test]
fn hello_plugin_applies_and_provides_a_service() {
    let root = Context::root();
    let fork = root.fork();
    let config = HelloConfig {
        message: "你好，dsh!".into(),
    };

    HelloPlugin.apply(&fork, &config).unwrap();

    let greeting: Arc<Greeting> = fork.get().unwrap();
    assert_eq!(greeting.message, "你好，dsh!");

    // 卸载：服务随 fiber 反注册
    fork.fiber().dispose();
    assert!(matches!(
        fork.get::<Greeting>(),
        Err(CoreError::ServiceNotFound("greeting"))
    ));
}
