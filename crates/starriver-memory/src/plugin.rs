// cordis plugin mount — the `starriver-tools::in_memory_tools_plugin` pattern.
//
// Phase 1: provides the shared `Arc<MemoryStore>` into the DI context so the
// host (and later the memory tools / ingestion pipeline in the same plugin)
// all reference ONE store instance. Tools, prompt-section injection, and the
// loop-exit extraction hook are layered onto this plugin in later phases.

use std::path::PathBuf;
use std::sync::Arc;

use cordis_rslite::{Context, EventBus, Plugin};

use crate::store::{MemoryConfig, MemoryStore};

pub fn memory_plugin(root: PathBuf, cfg: MemoryConfig) -> impl Plugin {
    struct MemoryPlugin {
        root: PathBuf,
        cfg: MemoryConfig,
    }

    impl Plugin for MemoryPlugin {
        fn name(&self) -> &'static str {
            "starriver-memory"
        }

        fn apply(
            &self,
            ctx: &Context,
            _bus: &EventBus,
        ) -> cordis_rslite::BoxFuture<anyhow::Result<Vec<cordis_rslite::EffectHandle>>> {
            let root = self.root.clone();
            let cfg = self.cfg.clone();
            let ctx = ctx.clone();
            Box::pin(async move {
                let effect = ctx.provide::<MemoryStore>(Arc::new(MemoryStore::new(root, cfg)))?;
                Ok(vec![effect])
            })
        }
    }

    MemoryPlugin { root, cfg }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cordis_rslite::PluginRegistry;

    #[tokio::test]
    async fn plugin_provides_shared_store() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PluginRegistry::new();
        registry.register(memory_plugin(dir.path().join("m"), MemoryConfig::default()));
        let ctx = Context::new();
        let bus = EventBus::new();
        let handles = registry.boot(&ctx, &bus).await.unwrap();
        let store = ctx.require::<MemoryStore>().unwrap();
        assert_eq!(store.root(), dir.path().join("m"));
        for h in handles {
            h.shutdown().await;
        }
    }
}
