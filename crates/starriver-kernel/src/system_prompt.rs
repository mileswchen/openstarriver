//! System-prompt assembly seam: ordered sections (static or dynamic),
//! `{{variable}}` interpolation with strict unknown-reference errors,
//! global+scoped dual tables with same-name shadowing, `complete:true`
//! whole-prompt replacement, and runtime-context suppressors.
//!
//! Mirrors `packages/core/system-prompt`: no cache or invalidation
//! every [`SystemPromptRegistry::assemble`] recomputes everything; stability
//! of the rendered system comes from idempotent inputs, which is what keeps
//! provider-side prompt caches warm.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EffectHandle};

use crate::context::{PromptContext, RenderContext};

#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    #[error("section `{0}` is already registered in this scope")]
    DuplicateSection(String),
    #[error("variable `{0}` is already registered in this scope")]
    DuplicateVariable(String),
    #[error("invalid variable name `{0}`: must match [a-z][a-z0-9_]*")]
    InvalidVariableName(String),
    #[error("unknown variable reference `{0}`")]
    UnknownVariable(String),
    #[error("multiple complete:true sections are active: {0}")]
    MultipleComplete(String),
}

/// Section body: static text (interpolated) or an evaluated function (used
/// verbatim — a dynamic provider owns its formatting).
pub enum SectionText {
    Static(String),
    Dynamic(Arc<dyn Fn(&RenderContext) -> String + Send + Sync>),
}

pub type VariableProvider = Arc<dyn Fn(&RenderContext) -> String + Send + Sync>;

/// A registered system section. `complete:true` replaces every other section
/// while active; two simultaneously active complete sections are an error.
pub struct SectionEntry {
    pub name: String,
    pub order: i32,
    pub complete: bool,
    pub text: SectionText,
}

pub struct VariableEntry {
    pub name: String,
    pub provider: VariableProvider,
}

#[derive(Default)]
struct Layer {
    /// BUG-23: a BTreeMap iterates by NAME, so equal-order ties need an
    /// explicit registration sequence to stay insertion-stable (this struct's
    /// own contract). Entries are write-once (re-registration is rejected),
    /// and removal drops the sequence so a later re-registration takes a
    /// fresh slot.
    sections: BTreeMap<String, SectionEntry>,
    section_seq: BTreeMap<String, u64>,
    variables: BTreeMap<String, VariableEntry>,
    contexts: BTreeMap<String, PromptContext>,
    context_seq: BTreeMap<String, u64>,
    next_seq: u64,
    suppressors: usize,
}

impl Layer {
    fn take_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

struct RegistryInner {
    global: Mutex<Layer>,
    scoped: Mutex<HashMap<String, Layer>>,
}

/// Prompt composition toggles (LLP settings namespace).
#[derive(Debug, Clone)]
pub struct PromptSettings {
    /// `false` → suppress the `-100` identity resident.
    pub include_identity: bool,
    /// `false` → suppress runtime-context snapshots (sandbox/approval/time).
    pub include_runtime_context: bool,
    /// `-99` workspace source declaration (OSS/checkout only).
    pub source_path: Option<String>,
    /// Platform baseline persona (order 0, below per-agent persona).
    pub persona_global: Option<String>,
    /// Composite workspace-instructions byte budget (`None` = the engine
    /// default, 64 KiB).
    pub instructions_max_bytes: Option<usize>,
 /// Pin the reply language (`language`-section parity). `None`
    /// → the `language` resident asks the model to follow the user's messages.
    pub response_language: Option<String>,
 /// Presentation order for `<tool>:guide` sections (`toolOrder`
    /// parity). Must contain `<unlisted-tools>` exactly once when set; the
    /// assembler rejects otherwise.
    pub tool_order: Option<Vec<String>>,
}

impl Default for PromptSettings {
    fn default() -> Self {
        Self {
            include_identity: true,
            include_runtime_context: true,
            source_path: None,
            persona_global: None,
            instructions_max_bytes: None,
            response_language: None,
            tool_order: None,
        }
    }
}

/// Reserved order band for per-tool guide sections (`<tool>:guide`), the
/// tool-guide family. The host registers the first-party set at boot
/// (bash/read/write/edit/glob/grep/terminal/web_search/web_fetch) — one
/// division-of-labor line per family, `tool:<name>` parity.
pub const TOOL_GUIDE_ORDER_MIN: i32 = 100;
/// Inclusive upper bound of the tool-guide band.
pub const TOOL_GUIDE_ORDER_MAX: i32 = 199;

/// -parity sentinel naming the position where guide sections of tools NOT
/// listed in a [`ToolOrderHint`] render (after every listed tool, keeping
/// registration order among themselves).
pub const UNLISTED_TOOLS: &str = "<unlisted-tools>";

/// -parity seam for tool-guide ordering (`toolOrder` config +
/// `<unlisted-tools>` placeholder): tool names in declared presentation
/// order. The host wires it through `prompt.tool_order` (settings) → the
/// loop's `assemble_with_tool_order` call; an order that fails validation
/// falls back to the default (registration-order) assembly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolOrderHint {
    /// Tool names whose `<tool>:guide` sections render in this order.
    pub order: Vec<String>,
}

impl ToolOrderHint {
    /// Presentation position of `tool`, or `None` for an unlisted tool (the
    /// `<unlisted-tools>` block after every listed name).
    pub fn position(&self, tool: &str) -> Option<usize> {
        self.order.iter().position(|name| name == tool)
    }
}

/// The `<tool>` half of a `<tool>:guide` section name, when the name carries
/// the guide suffix.
fn tool_of_guide_section(name: &str) -> Option<&str> {
    name.strip_suffix(":guide")
}

/// Stable reorder of the tool-guide band: sections named `<tool>:guide` with
/// order in `100..=199` render in hint order (listed first, then unlisted in
/// insertion order), spliced back into the exact slot indices the band
/// occupied so everything outside the band is untouched.
fn apply_tool_order(sections: &mut [EvaluatedSection], hint: &ToolOrderHint) {
    let slots: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter(|(_, section)| {
            (TOOL_GUIDE_ORDER_MIN..=TOOL_GUIDE_ORDER_MAX).contains(&section.order)
                && tool_of_guide_section(&section.name).is_some()
        })
        .map(|(index, _)| index)
        .collect();
    if slots.len() < 2 {
        return;
    }
    let mut band: Vec<(usize, EvaluatedSection)> = slots
        .iter()
        .map(|&index| (index, sections[index].clone()))
        .collect();
    // Sort by (listed-position-or-after, original slot) — stable for equal
    // keys by construction, so unlisted guides keep insertion order.
    band.sort_by_key(|(slot, section)| {
        let listed = tool_of_guide_section(&section.name)
            .and_then(|tool| hint.position(tool))
            .unwrap_or(hint.order.len());
        (listed, *slot)
    });
    for &slot in &slots {
        sections[slot] = band.remove(0).1;
    }
}

/// Thread-safe scoped registry: a global layer plus per-agent layers. A
/// scoped entry shadows the same-named global entry instead of stacking.
#[derive(Clone)]
pub struct SystemPromptRegistry {
    inner: Arc<RegistryInner>,
}

/// The assembled output for one step: the final interpolated system string
/// plus the current runtime-context contributions (empty when suppressed).
#[derive(Debug)]
pub struct AssembledPrompt {
    pub system: String,
    pub contexts: Vec<ContextOut>,
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug)]
pub struct ContextOut {
    pub name: String,
    pub text: String,
}

impl From<ContextOut> for crate::context::AssembledContext {
    fn from(out: ContextOut) -> Self {
        Self {
            name: out.name,
            text: out.text,
        }
    }
}

impl SectionText {
    fn eval(&self, ctx: &RenderContext<'_>) -> String {
        match self {
            SectionText::Static(text) => text.clone(),
            SectionText::Dynamic(function) => function(ctx),
        }
    }
}

fn validate_variable_name(name: &str) -> Result<(), AssembleError> {
    let mut chars = name.chars();
    let valid = matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(AssembleError::InvalidVariableName(name.into()))
    }
}

/// One evaluated entry kept in assembled order: name survives for errors.
#[derive(Clone)]
struct EvaluatedSection {
    name: String,
    order: i32,
    /// 0 = global layer, 1 = scoped shadow (equal order: global first).
    tier: u8,
    seq: usize,
    complete: bool,
    text: String,
}

impl SystemPromptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_layer<R>(&self, agent_ctx: &Context, edit: impl FnOnce(&mut Layer) -> R) -> R {
        let tag = agent_ctx.tag().map(|t| t.to_string());
        match tag {
            Some(tag) => {
                let mut scoped = self.inner.scoped.lock().expect("system prompt poisoned");
                edit(scoped.entry(tag).or_default())
            }
            None => edit(&mut self.inner.global.lock().expect("system prompt poisoned")),
        }
    }

    /// Register an ordered system section. Ownership: the returned handle
    /// keeps the entry alive until [`EffectHandle::dispose_async`] — dropping
    /// it without disposal only silences the borrow warning (cordis contract,
 /// same as listeners); use `.forget` for permanently-live entries.
    pub fn section(
        &self,
        agent_ctx: &Context,
        entry: SectionEntry,
    ) -> Result<EffectHandle, AssembleError> {
        let name = entry.name.clone();
        self.with_layer(agent_ctx, |layer| {
            if layer.sections.contains_key(&name) {
                return Err(AssembleError::DuplicateSection(name.clone()));
            }
            let seq = layer.take_seq();
            layer.section_seq.insert(name.clone(), seq);
            layer.sections.insert(name.clone(), entry);
            Ok(())
        })?;
        Ok(self.removal_for(agent_ctx, {
            let remove_name = name.clone();
            move |layer| {
                layer.sections.remove(&remove_name);
                layer.section_seq.remove(&remove_name);
            }
        }))
    }

    /// Register a named variable provider.
    pub fn variable(
        &self,
        agent_ctx: &Context,
        entry: VariableEntry,
    ) -> Result<EffectHandle, AssembleError> {
        validate_variable_name(&entry.name)?;
        let name = entry.name.clone();
        self.with_layer(agent_ctx, |layer| {
            if layer.variables.contains_key(&name) {
                return Err(AssembleError::DuplicateVariable(name.clone()));
            }
            layer.variables.insert(name.clone(), entry);
            Ok(())
        })?;
        Ok(self.removal_for(agent_ctx, {
            let remove_name = name.clone();
            move |layer| {
                layer.variables.remove(&remove_name);
            }
        }))
    }

    /// Register a runtime-context contribution (rendered into the pre-step
    /// history snapshot, never into the system string).
    pub fn context(
        &self,
        agent_ctx: &Context,
        entry: PromptContext,
    ) -> Result<EffectHandle, AssembleError> {
        let name = entry.name.clone();
        self.with_layer(agent_ctx, |layer| {
            if layer.contexts.contains_key(&name) {
                return Err(AssembleError::DuplicateSection(name.clone()));
            }
            let seq = layer.take_seq();
            layer.context_seq.insert(name.clone(), seq);
            layer.contexts.insert(name.clone(), entry);
            Ok(())
        })?;
        Ok(self.removal_for(agent_ctx, {
            let remove_name = name.clone();
            move |layer| {
                layer.contexts.remove(&remove_name);
                layer.context_seq.remove(&remove_name);
            }
        }))
    }

    /// Register one anonymous runtime-context suppressor. Any live suppressor
    /// on the global layer or on the assembled scope clears all contexts.
    pub fn suppress_runtime_context(&self, agent_ctx: &Context) -> EffectHandle {
        self.with_layer(agent_ctx, |layer| layer.suppressors += 1);
        self.removal_for(agent_ctx, |layer| {
            layer.suppressors = layer.suppressors.saturating_sub(1);
        })
    }

    /// Tag-aware variant used internally so scoped removals stay scoped.
    fn removal_for(
        &self,
        agent_ctx: &Context,
        remove: impl Fn(&mut Layer) + Send + Sync + 'static,
    ) -> EffectHandle {
        let tag = agent_ctx.tag().map(|t| t.to_string());
        match tag {
            Some(tag) => {
                let inner = Arc::clone(&self.inner);
                EffectHandle::new(move || {
                    if let Some(layer) = inner
                        .scoped
                        .lock()
                        .expect("system prompt poisoned")
                        .get_mut(&tag)
                    {
                        remove(layer);
                    }
                })
            }
            None => {
                let inner = Arc::clone(&self.inner);
                EffectHandle::new(move || {
                    remove(&mut inner.global.lock().expect("system prompt poisoned"));
                })
            }
        }
    }

    /// Full recompute — no caching by design. Deterministic: identical inputs
    /// produce a byte-identical system string.
    pub fn assemble(
        &self,
        agent_ctx: &Context,
        render: &RenderContext<'_>,
    ) -> Result<AssembledPrompt, AssembleError> {
        // The default path carries no tool order (zero residents, zero
        // config — WI-34 reserved seam), so its output is byte-identical to
        // the pre-hook assembler.
        self.assemble_with_tool_order(agent_ctx, render, None)
    }

 /// [`Self::assemble`] with the reserved `toolOrder` seam: when a hint
    /// is supplied, `<tool>:guide` sections in the reserved 100..=199 band
    /// render in hint order (unlisted tools after listed ones, the
    /// `<unlisted-tools>` block); `None` leaves the assembly untouched.
    pub fn assemble_with_tool_order(
        &self,
        agent_ctx: &Context,
        render: &RenderContext<'_>,
        tool_order: Option<&ToolOrderHint>,
    ) -> Result<AssembledPrompt, AssembleError> {
        // Merge far-to-near: globals first, then same-named scoped entries
        // overwrite them (shadowing, not stacking). Evaluate while holding
        // both locks — evaluatees only read session/options.
        let global = self.inner.global.lock().expect("system prompt poisoned");
        let mut merged: Vec<EvaluatedSection> = Vec::new();
        let mut variables: BTreeMap<String, String> = BTreeMap::new();

        for (name, entry) in &global.sections {
            merged.push(EvaluatedSection {
                name: name.clone(),
                order: entry.order,
                tier: 0,
                seq: global.section_seq.get(name).copied().unwrap_or(0) as usize,
                complete: entry.complete,
                text: entry.text.eval(render),
            });
        }
        for (name, entry) in &global.variables {
            variables.insert(name.clone(), (entry.provider)(render));
        }

        let scope_tag = agent_ctx.tag().map(|t| t.to_string());
        let scope_suppressed = match scope_tag {
            Some(tag) => {
                let scoped = self.inner.scoped.lock().expect("system prompt poisoned");
                match scoped.get(&tag) {
                    Some(layer) => {
                        merged.retain(|section| !layer.sections.contains_key(&section.name));
                        for (name, entry) in &layer.sections {
                            merged.push(EvaluatedSection {
                                name: name.clone(),
                                order: entry.order,
                                tier: 1,
                                seq: layer.section_seq.get(name).copied().unwrap_or(0) as usize,
                                complete: entry.complete,
                                text: entry.text.eval(render),
                            });
                        }
                        for name in layer.variables.keys() {
                            variables.remove(name);
                        }
                        for (name, entry) in &layer.variables {
                            variables.insert(name.clone(), (entry.provider)(render));
                        }
                        layer.suppressors > 0
                    }
                    None => false,
                }
            }
            None => false,
        };
        let suppressed = global.suppressors > 0 || scope_suppressed;
        drop(global);

        // (order, tier, registration seq): order first; at equal order the
        // global layer renders before scoped shadows (far-to-near merge);
        // within a layer, registration order breaks ties (BUG-23).
        merged.sort_by_key(|section| (section.order, section.tier, section.seq));

        // Reserved toolOrder seam (WI-34): applied only when a caller
        // supplies a hint; the default path (None) never reaches here.
        if let Some(hint) = tool_order {
            apply_tool_order(&mut merged, hint);
        }

        // Complete resolution precedes interpolation.
        let completes: Vec<&EvaluatedSection> =
            merged.iter().filter(|section| section.complete).collect();
        if completes.len() > 1 {
            let names = completes
                .iter()
                .map(|section| format!("\"{}\"", section.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AssembleError::MultipleComplete(names));
        }

        let system = match completes.first() {
            Some(section) => interpolate(&section.text, &variables)?,
            None => {
                // Sections that evaluate to nothing (a dynamic provider with
                // no content this step) drop out entirely so an absent
                // resident leaves no separator trace in the system string.
                let joined = merged
                    .iter()
                    .map(|section| section.text.as_str())
                    .filter(|text| !text.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                interpolate(&joined, &variables)?
            }
        };

        let contexts = if suppressed {
            Vec::new()
        } else {
            self.collect_contexts(agent_ctx, render)
        };

        Ok(AssembledPrompt {
            system,
            contexts,
            variables,
        })
    }

    fn collect_contexts(&self, agent_ctx: &Context, render: &RenderContext<'_>) -> Vec<ContextOut> {
        let global = self.inner.global.lock().expect("system prompt poisoned");
        // Contributions that evaluate to nothing are not active this step and
        // drop out entirely — two silent contributors must not leave a "\n\n"
        // seam in the snapshot face.
        let mut ordered: Vec<(u32, u8, u64, ContextOut)> = Vec::new();
        for (name, entry) in &global.contexts {
            let text = (entry.render)(render);
            if text.is_empty() {
                continue;
            }
            ordered.push((
                entry.order,
                0,
                global.context_seq.get(name).copied().unwrap_or(0),
                ContextOut {
                    name: entry.name.clone(),
                    text,
                },
            ));
        }
        let scope_tag = agent_ctx.tag().map(|t| t.to_string());
        if let Some(tag) = scope_tag {
            let scoped = self.inner.scoped.lock().expect("system prompt poisoned");
            if let Some(layer) = scoped.get(&tag) {
                ordered.retain(|entry| !layer.contexts.contains_key(&entry.3.name));
                for (name, entry) in &layer.contexts {
                    let text = (entry.render)(render);
                    if text.is_empty() {
                        continue;
                    }
                    ordered.push((
                        entry.order,
                        1,
                        layer.context_seq.get(name).copied().unwrap_or(0),
                        ContextOut {
                            name: entry.name.clone(),
                            text,
                        },
                    ));
                }
            }
        }
        drop(global);
        // Same key shape as the section sort: order, then layer (global
        // before scoped shadows), then registration order within the layer.
        ordered.sort_by_key(|(order, tier, seq, _)| (*order, *tier, *seq));
        ordered.into_iter().map(|(_, _, _, out)| out).collect()
    }
}

impl Default for SystemPromptRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                global: Mutex::new(Layer::default()),
                scoped: Mutex::new(HashMap::new()),
            }),
        }
    }
}

/// Substitute `{{name}}` references with resolved values. Unknown references
/// error; malformed or lone `{{` stays literal text.
fn interpolate(text: &str, vars: &BTreeMap<String, String>) -> Result<String, AssembleError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = &after[..end];
                if validate_variable_name(name).is_ok() {
                    let value = vars
                        .get(name)
                        .ok_or_else(|| AssembleError::UnknownVariable(name.to_string()))?;
                    out.push_str(value);
                    rest = &after[end + 2..];
                } else {
                    out.push_str("{{");
                    rest = after;
                }
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentOptions;
    use crate::session::{Session, SessionId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Per-test fixture: owns its registry + render context pair.
    struct Fixture {
        reg: SystemPromptRegistry,
        global_ctx: Context,
        session: Session,
        options: AgentOptions,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                reg: SystemPromptRegistry::new(),
                global_ctx: Context::new(),
                session: Session::new(SessionId("t".into()), cordis_rslite::EventBus::new()),
                options: AgentOptions::default(),
            }
        }
        fn scoped(&self, tag: &str) -> (Context, RenderContext<'_>) {
            let agent = self.global_ctx.clone().tagged(Arc::from(tag));
            let render = RenderContext {
                session: &self.session,
                options: &self.options,
            };
            (agent, render)
        }
        fn as_view(&self) -> (&Context, RenderContext<'_>) {
            (
                &self.global_ctx,
                RenderContext {
                    session: &self.session,
                    options: &self.options,
                },
            )
        }
        fn section(&self, name: &str, order: i32, text: &str) -> EffectHandle {
            self.reg
                .section(
                    &self.global_ctx,
                    SectionEntry {
                        name: name.into(),
                        order,
                        complete: false,
                        text: SectionText::Static(text.into()),
                    },
                )
                .expect("register section")
        }
    }

    #[test]
    fn empty_evaluated_sections_leave_no_separator_trace() {
        // A dynamic provider that renders nothing (e.g. no always-on skills)
        // must not leave a stray "\n\n" seam between its neighbours.
        let f = Fixture::new();
        f.section("a", 0, "head");
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "empty".into(),
                    order: 50,
                    complete: false,
                    text: SectionText::Dynamic(Arc::new(|_| String::new())),
                },
            )
            .unwrap();
        f.section("z", 100, "tail");
        let (agent, render) = f.as_view();
        assert_eq!(
            f.reg.assemble(agent, &render).unwrap().system,
            "head\n\ntail"
        );
    }

    #[tokio::test]
    async fn context_out_converts_to_assembled_context() {
        let f = Fixture::new();
        f.reg
            .context(
                &f.global_ctx,
                crate::context::PromptContext {
                    name: "cwd".into(),
                    order: 1,
                    render: Arc::new(|_| "/tmp".into()),
                },
            )
            .unwrap();
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        let converted: Vec<crate::context::AssembledContext> =
            out.contexts.into_iter().map(Into::into).collect();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, "cwd");
        assert_eq!(converted[0].text, "/tmp");
    }

    #[test]
    fn empty_context_contributions_are_dropped() {
        let f = Fixture::new();
        for (name, order, text) in [
            ("silent-a", 1u32, ""),
            ("live", 2u32, "root=/w"),
            ("silent-b", 3u32, ""),
        ] {
            let owned = text.to_string();
            let _ = f.reg.context(
                &f.global_ctx,
                crate::context::PromptContext {
                    name: name.into(),
                    order,
                    render: Arc::new(move |_| owned.clone()),
                },
            );
        }
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert_eq!(
            out.contexts
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["live"],
            "silent contributors must not survive — two of them would leave a \
             separator-only snapshot body"
        );
    }

    #[test]
    fn sections_concatenate_in_order_and_interpolate() {
        let f = Fixture::new();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "b".into(),
                    order: 10,
                    complete: false,
                    text: SectionText::Static("second {{model}}".into()),
                },
            )
            .unwrap();
        f.section("a", -100, "first");
        f.reg
            .variable(
                &f.global_ctx,
                VariableEntry {
                    name: "model".into(),
                    provider: Arc::new(|_| "deepseek-v4".into()),
                },
            )
            .unwrap();

        let (_, render) = f.as_view();
        let out = f.reg.assemble(&f.global_ctx, &render).unwrap();
        assert_eq!(out.system, "first\n\nsecond deepseek-v4");
        assert_eq!(out.variables["model"], "deepseek-v4");
    }

    #[test]
    fn scoped_section_shadows_global_same_name() {
        let f = Fixture::new();
        f.section("deployment:persona", 0, "GLOBAL");
        let (scoped_agent, scoped_render) = f.scoped("agent-1");
        f.reg
            .section(
                &scoped_agent,
                SectionEntry {
                    name: "deployment:persona".into(),
                    order: 0,
                    complete: false,
                    text: SectionText::Static("AGENT".into()),
                },
            )
            .unwrap();

        let out = f.reg.assemble(&scoped_agent, &scoped_render).unwrap();
        assert_eq!(out.system, "AGENT", "scoped shadows same-named global");

        let (plain_agent, plain_render) = f.as_view();
        let out = f.reg.assemble(plain_agent, &plain_render).unwrap();
        assert_eq!(out.system, "GLOBAL", "global view keeps the base persona");
    }

    #[test]
    fn dynamic_sections_evaluate_on_every_assemble() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter2 = Arc::clone(&counter);
        let closure: Arc<dyn Fn(&RenderContext) -> String + Send + Sync> =
            Arc::new(move |_| (counter2.fetch_add(1, Ordering::SeqCst) + 1).to_string());

        let f = Fixture::new();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "counter".into(),
                    order: 0,
                    complete: false,
                    text: SectionText::Dynamic(closure),
                },
            )
            .unwrap();

        let (agent, first) = f.as_view();
        let _ = f.reg.assemble(agent, &first).unwrap();
        let (_agent2, second) = f.as_view();
        let _ = f.reg.assemble(_agent2, &second).unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2, "no caching by design");
    }

    #[test]
    fn unknown_variable_reference_is_an_error() {
        let f = Fixture::new();
        f.section("s", 0, "hello {{who}}");
        let (agent, render) = f.as_view();
        let err = f.reg.assemble(agent, &render).unwrap_err();
        assert!(matches!(err, AssembleError::UnknownVariable(ref n) if n == "who"));
    }

    #[test]
    fn malformed_or_lone_braces_stay_literal() {
        let f = Fixture::new();
        f.section("s", 0, "{{ 1 }} keep {{unclosed and {{BadName}}");
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert_eq!(out.system, "{{ 1 }} keep {{unclosed and {{BadName}}");
    }

    #[test]
    fn invalid_variable_names_are_rejected_at_registration() {
        let f = Fixture::new();
        for bad in ["9bad", "has-dash", "Has_Caps"] {
            assert!(
                f.reg
                    .variable(
                        &f.global_ctx,
                        VariableEntry {
                            name: bad.into(),
                            provider: Arc::new(|_| String::new()),
                        }
                    )
                    .is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn duplicate_registration_in_scope_is_rejected() {
        let f = Fixture::new();
        f.section("dup", 0, "one");
        assert!(matches!(
            f.reg.section(
                &f.global_ctx,
                SectionEntry {
                    name: "dup".into(),
                    order: 1,
                    complete: false,
                    text: SectionText::Static("two".into()),
                }
            ),
            Err(AssembleError::DuplicateSection(_))
        ));
    }

    #[test]
    fn two_complete_sections_are_a_conflict() {
        let f = Fixture::new();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "a".into(),
                    order: 0,
                    complete: true,
                    text: SectionText::Static("A".into()),
                },
            )
            .unwrap();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "b".into(),
                    order: 1,
                    complete: true,
                    text: SectionText::Static("B".into()),
                },
            )
            .unwrap();
        let (agent, render) = f.as_view();
        let err = f.reg.assemble(agent, &render).unwrap_err();
        assert!(matches!(err, AssembleError::MultipleComplete(_)));
    }

    #[test]
    fn single_complete_replaces_everything() {
        let f = Fixture::new();
        f.section("persona", 0, "persona text");
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "review".into(),
                    order: 80,
                    complete: true,
                    text: SectionText::Static("**REVIEW MODE**".into()),
                },
            )
            .unwrap();
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert_eq!(out.system, "**REVIEW MODE**");
    }

    /// Registration handles follow the cordis ownership contract: the entry
    /// lives until the handle is disposed (`forget()` keeps it forever).
    #[tokio::test]
    async fn suppression_clears_contexts_and_dispose_restores_them() {
        let f = Fixture::new();
        f.reg
            .context(
                &f.global_ctx,
                PromptContext {
                    name: "cwd".into(),
                    order: 1,
                    render: Arc::new(|_| "/Users/x/proj".into()),
                },
            )
            .unwrap();

        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert_eq!(out.contexts[0].text, "/Users/x/proj");

        let handle = f.reg.suppress_runtime_context(&f.global_ctx);
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert!(out.contexts.is_empty(), "suppressed contexts must clear");

        handle.dispose_async().await;
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert_eq!(out.contexts.len(), 1, "disposing the suppressor restores");
    }

    #[tokio::test]
    async fn dispose_unregisters_the_section() {
        let f = Fixture::new();
        let handle = f.section("temp", 0, "here");
        let (agent, render) = f.as_view();
        assert_eq!(f.reg.assemble(agent, &render).unwrap().system, "here");

        handle.dispose_async().await;
        let (agent, render) = f.as_view();
        assert_eq!(
            f.reg.assemble(agent, &render).unwrap().system,
            "",
            "disposed registration is gone"
        );
    }

    #[test]
    fn scoped_variable_shadows_the_global_namesake() {
        let f = Fixture::new();
        f.reg
            .variable(
                &f.global_ctx,
                VariableEntry {
                    name: "model".into(),
                    provider: Arc::new(|_| "global-model".into()),
                },
            )
            .unwrap();
        let (scoped_agent, scoped_render) = f.scoped("agent-1");
        f.reg
            .variable(
                &scoped_agent,
                VariableEntry {
                    name: "model".into(),
                    provider: Arc::new(|_| "scoped-model".into()),
                },
            )
            .unwrap();
        f.section("uses", 0, "{{model}}");

        let out = f.reg.assemble(&scoped_agent, &scoped_render).unwrap();
        assert_eq!(out.system, "scoped-model", "scoped variable shadows global");

        let (plain_agent, plain_render) = f.as_view();
        let out = f.reg.assemble(plain_agent, &plain_render).unwrap();
        assert_eq!(
            out.system, "global-model",
            "global view keeps the base value"
        );
    }

    #[tokio::test]
    async fn scoped_suppressor_clears_only_its_own_scope() {
        let f = Fixture::new();
        f.reg
            .context(
                &f.global_ctx,
                crate::context::PromptContext {
                    name: "cwd".into(),
                    order: 1,
                    render: Arc::new(|_| "/w".into()),
                },
            )
            .unwrap();

        let (scoped_agent, scoped_render) = f.scoped("agent-1");
        let handle = f.reg.suppress_runtime_context(&scoped_agent);
        let out = f.reg.assemble(&scoped_agent, &scoped_render).unwrap();
        assert!(out.contexts.is_empty(), "suppressed scope sees no contexts");

        let (other_agent, other_render) = f.scoped("agent-2");
        let out = f.reg.assemble(&other_agent, &other_render).unwrap();
        assert_eq!(out.contexts.len(), 1, "other scopes keep their contexts");

        handle.dispose_async().await;
        let (agent, render) = f.scoped("agent-1");
        let out = f.reg.assemble(&agent, &render).unwrap();
        assert_eq!(out.contexts.len(), 1, "disposing restores the scope");
    }

    #[test]
    fn multiple_complete_error_names_every_conflicting_section() {
        let f = Fixture::new();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "mode:review".into(),
                    order: 80,
                    complete: true,
                    text: SectionText::Static("R".into()),
                },
            )
            .unwrap();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "mode:plan".into(),
                    order: 81,
                    complete: true,
                    text: SectionText::Static("P".into()),
                },
            )
            .unwrap();
        let (agent, render) = f.as_view();
        let err = f.reg.assemble(agent, &render).unwrap_err();
        let AssembleError::MultipleComplete(names) = err else {
            panic!("expected MultipleComplete, got {err:?}")
        };
        assert!(
            names.contains("\"mode:review\"") && names.contains("\"mode:plan\""),
            "both conflicting names must be listed: {names}"
        );
    }

    #[test]
    fn complete_section_still_interpolates_variables() {
        let f = Fixture::new();
        f.reg
            .variable(
                &f.global_ctx,
                VariableEntry {
                    name: "cwd".into(),
                    provider: Arc::new(|_| "/w/proj".into()),
                },
            )
            .unwrap();
        f.reg
            .section(
                &f.global_ctx,
                SectionEntry {
                    name: "review".into(),
                    order: 80,
                    complete: true,
                    text: SectionText::Static("review {{cwd}}".into()),
                },
            )
            .unwrap();
        let (agent, render) = f.as_view();
        assert_eq!(
            f.reg.assemble(agent, &render).unwrap().system,
            "review /w/proj"
        );
    }

    #[test]
    fn equal_order_contexts_keep_insertion_order() {
        let f = Fixture::new();
        for (name, text) in [("first", "1"), ("second", "2"), ("third", "3")] {
            let owned = text.to_string();
            let _ = f.reg.context(
                &f.global_ctx,
                crate::context::PromptContext {
                    name: name.into(),
                    order: 50,
                    render: Arc::new(move |_| owned.clone()),
                },
            );
        }
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        let names: Vec<&str> = out.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["first", "second", "third"],
            "ties stay insertion-stable"
        );
    }

    /// BUG-23 regression: the tie contract is *insertion* order (Layer's own
    /// doc comment), but the fixture above is vacuous — "first/second/third"
    /// is also lexicographic, so a name-sorted BTreeMap passes it too. With
    /// names whose lexicographic order differs from registration order the
    /// two semantics diverge; this pins the documented one.
    #[test]
    fn equal_order_ties_follow_registration_not_lexicographic() {
        let f = Fixture::new();
        f.section("zeta", 0, "Z-registered-first");
        f.section("alpha", 0, "A-registered-second");
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        assert!(
            out.system.starts_with("Z-registered-first"),
            "equal-order sections must render in registration order, got: {}",
            out.system
        );

        for (name, text) in [("zz-context", "ZZ"), ("aa-context", "AA")] {
            let owned = text.to_string();
            let _ = f.reg.context(
                &f.global_ctx,
                crate::context::PromptContext {
                    name: name.into(),
                    order: 50,
                    render: Arc::new(move |_| owned.clone()),
                },
            );
        }
        let (agent, render) = f.as_view();
        let out = f.reg.assemble(agent, &render).unwrap();
        let names: Vec<&str> = out.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["zz-context", "aa-context"],
            "equal-order contexts must keep registration order, not name order"
        );
    }

    #[test]
    fn tool_order_hint_type_and_sentinel_are_reserved() {
 // WI-34 structural reservation: the type/sentinel exist with
        // semantics (listed position; unlisted after listed), while nothing
        // registers residents or reads configuration for them.
        let hint = ToolOrderHint {
            order: vec!["bash".into(), "fs".into()],
        };
        assert_eq!(hint.position("bash"), Some(0));
        assert_eq!(hint.position("fs"), Some(1));
        assert_eq!(hint.position("web"), None, "unlisted → <unlisted-tools>");
        assert_eq!(UNLISTED_TOOLS, "<unlisted-tools>");
        assert_eq!(TOOL_GUIDE_ORDER_MIN, 100);
        assert_eq!(TOOL_GUIDE_ORDER_MAX, 199);
        assert_eq!(
            ToolOrderHint::default(),
            ToolOrderHint { order: Vec::new() }
        );
    }

    #[test]
    fn default_assemble_is_byte_identical_to_hintless_extended_path() {
        // The default path must be byte-for-byte the pre-hook assembly: a
        // mixed fixture (residents + a two-section guide band) renders
        // identically through assemble() and assemble_with_tool_order(None).
        let f = Fixture::new();
        f.section("identity", -100, "who");
        f.section("bash:guide", 100, "use bash");
        f.section("fs:guide", 101, "use fs");
        f.section("persona", 0, "persona text");
        let (agent, render) = f.as_view();
        let via_default = f.reg.assemble(agent, &render).unwrap();
        let via_extended = f
            .reg
            .assemble_with_tool_order(agent, &render, None)
            .unwrap();
        assert_eq!(via_default.system, via_extended.system);
        assert_eq!(
            via_default.system, "who\n\npersona text\n\nuse bash\n\nuse fs",
            "guide band keeps (order, seq) ordering without a hint"
        );
    }

    #[test]
    fn tool_order_hint_reorders_only_the_guide_band() {
        let f = Fixture::new();
        f.section("identity", -100, "who");
        f.section("fs:guide", 100, "use fs");
        f.section("bash:guide", 100, "use bash");
        f.section("web:guide", 100, "use web");
        f.section("persona", 0, "persona text");
        let hint = ToolOrderHint {
            order: vec!["web".into(), "bash".into()],
        };
        let (agent, render) = f.as_view();
        let out = f
            .reg
            .assemble_with_tool_order(agent, &render, Some(&hint))
            .unwrap();
        assert_eq!(
            out.system, "who\n\npersona text\n\nuse web\n\nuse bash\n\nuse fs",
            "listed guides render in hint order; unlisted (fs) follows in \
             insertion order; everything outside the band is untouched"
        );
    }

    #[test]
    fn tool_order_hint_leaves_non_guide_sections_in_the_band_alone() {
        let f = Fixture::new();
        // `plain` sits inside the reserved order band but is not a
        // `<tool>:guide` name — it must keep its (order, seq) position.
        f.section("plain", 100, "plain text");
        f.section("bash:guide", 150, "use bash");
        f.section("fs:guide", 150, "use fs");
        let hint = ToolOrderHint {
            order: vec!["fs".into(), "bash".into()],
        };
        let (agent, render) = f.as_view();
        let out = f
            .reg
            .assemble_with_tool_order(agent, &render, Some(&hint))
            .unwrap();
        assert_eq!(
            out.system, "plain text\n\nuse fs\n\nuse bash",
            "only <tool>:guide names participate; band-adjacent non-guide \
             sections keep (order, seq) order"
        );
    }
}
