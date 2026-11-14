//! Independent production entry inventory; cfg(test) fixtures are not producers.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::{
    parse::Parser,
    punctuated::Punctuated,
    visit::{self, Visit},
    Token,
};

// Unknown cfg predicates are included (both supported OS module graphs), but a
// positive cfg(test) cannot hide production code following the test module.
fn cfg_value(meta: &syn::Meta) -> Option<bool> {
    match meta {
        syn::Meta::Path(path) if path.is_ident("test") => Some(false),
        syn::Meta::List(list) => {
            let children = Punctuated::<syn::Meta, Token![,]>::parse_terminated
                .parse2(list.tokens.clone())
                .ok()?;
            let values: Vec<_> = children.iter().map(cfg_value).collect();
            if list.path.is_ident("not") {
                values.first().copied().flatten().map(|v| !v)
            } else if list.path.is_ident("all") {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|v| *v == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if list.path.is_ident("any") {
                if values.contains(&Some(true)) {
                    Some(true)
                } else if values.iter().all(|v| *v == Some(false)) {
                    Some(false)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    }
}
fn production(attrs: &[syn::Attribute]) -> bool {
    !attrs.iter().any(|a| {
        a.path().is_ident("test")
            || a.path().is_ident("cfg")
                && a.parse_args::<syn::Meta>()
                    .ok()
                    .as_ref()
                    .and_then(cfg_value)
                    == Some(false)
    })
}
struct Inventory {
    root: PathBuf,
    file: PathBuf,
    module_dir: PathBuf,
    function: String,
    files: BTreeSet<PathBuf>,
    constructors: BTreeMap<String, usize>,
    controls: BTreeSet<String>,
    aliases: BTreeSet<String>,
    factory_calls: BTreeSet<String>,
    includes: BTreeSet<String>,
}
impl Inventory {
    fn site(&self) -> String {
        format!(
            "{}::{}",
            self.file
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            self.function
        )
    }
    fn scan(&mut self, path: PathBuf, module_dir: PathBuf) {
        let path = path.canonicalize().unwrap();
        if !self.files.insert(path.clone()) {
            return;
        }
        let tree = syn::parse_file(&std::fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut aliases = AliasCollector::default();
        aliases.visit_file(&tree);
        self.aliases.extend(aliases.0);
        let old = (self.file.clone(), self.module_dir.clone());
        self.file = path;
        self.module_dir = module_dir;
        self.visit_file(&tree);
        self.file = old.0;
        self.module_dir = old.1;
    }
}
#[derive(Default)]
struct AliasCollector(BTreeSet<String>);
impl<'ast> Visit<'ast> for AliasCollector {
    fn visit_use_rename(&mut self, node: &'ast syn::UseRename) {
        if node.ident == "NewInput" {
            self.0.insert(node.rename.to_string());
        }
    }
}
fn token_mentions_input(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(id) => id == "NewInput",
        proc_macro2::TokenTree::Group(group) => token_mentions_input(group.stream()),
        _ => false,
    })
}
impl<'ast> Visit<'ast> for Inventory {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if !production(&node.attrs) {
            return;
        }
        if let Some((_, items)) = &node.content {
            let old = self.module_dir.clone();
            self.module_dir.push(node.ident.to_string());
            for item in items {
                self.visit_item(item);
            }
            self.module_dir = old;
        } else {
            let explicit = node.attrs.iter().find_map(|a| {
                if a.path().is_ident("path") {
                    if let syn::Meta::NameValue(v) = &a.meta {
                        if let syn::Expr::Lit(v) = &v.value {
                            if let syn::Lit::Str(v) = &v.lit {
                                return Some(v.value());
                            }
                        }
                    }
                    None
                } else {
                    None
                }
            });
            let (path, dir) = if let Some(path) = explicit {
                let path = self.file.parent().unwrap().join(path);
                let dir = path.parent().unwrap().to_path_buf();
                (path, dir)
            } else {
                let name = node.ident.to_string();
                let direct = self.module_dir.join(format!("{name}.rs"));
                let path = if direct.exists() {
                    direct
                } else {
                    self.module_dir.join(&name).join("mod.rs")
                };
                (path, self.module_dir.join(name))
            };
            self.scan(path, dir);
        }
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !production(&node.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_item_fn(self, node);
        self.function = old;
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !production(&node.attrs) {
            return;
        }
        let old = std::mem::replace(&mut self.function, node.sig.ident.to_string());
        visit::visit_impl_item_fn(self, node);
        self.function = old;
    }
    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        if production(&node.attrs) {
            visit::visit_item_macro(self, node);
        }
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node.path.is_ident("include") {
            let relative = syn::parse2::<syn::LitStr>(node.tokens.clone())
                .expect("production include must be a registered literal")
                .value();
            let path = self.file.parent().unwrap().join(relative);
            self.includes.insert(
                path.strip_prefix(&self.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
            self.scan(path, self.module_dir.clone());
            return;
        }
        assert!(
            !token_mentions_input(node.tokens.clone()),
            "register macro-generated input producers explicitly"
        );
        visit::visit_macro(self, node);
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if matches!(node.func.as_ref(),syn::Expr::Path(path) if path.path.segments.last().is_some_and(|s|matches!(s.ident.to_string().as_str(),"new_input"|"request_input"|"delivery_input")))
        {
            self.factory_calls.insert(self.site());
        }
        visit::visit_expr_call(self, node);
    }
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|s| self.aliases.contains(&s.ident.to_string()))
        {
            *self.constructors.entry(self.site()).or_default() += 1;
        }
        if node
            .fields
            .iter()
            .any(|f| matches!(&f.member,syn::Member::Named(name) if name=="control_type"))
        {
            self.controls.insert(self.site());
        }
        visit::visit_expr_struct(self, node);
    }
    fn visit_expr_assign(&mut self, node: &'ast syn::ExprAssign) {
        if matches!(node.left.as_ref(),syn::Expr::Field(field) if matches!(&field.member,syn::Member::Named(name) if name=="control_type"))
        {
            self.controls.insert(self.site());
        }
        visit::visit_expr_assign(self, node);
    }
}
#[test]
fn nextup_registered_producer_constructor_set_is_closed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let mut inventory = Inventory {
        root: root.clone(),
        file: root.clone(),
        module_dir: root.clone(),
        function: String::new(),
        files: Default::default(),
        constructors: Default::default(),
        controls: Default::default(),
        factory_calls: Default::default(),
        includes: Default::default(),
        aliases: BTreeSet::from(["NewInput".into()]),
    };
    for name in ["lib", "main"] {
        inventory.scan(root.join(format!("src/{name}.rs")), root.join("src"));
    }
    let expected: BTreeMap<_, _> = [
        ("src/asks/ledger.rs::request_input", 1),
        ("src/asks/ledger.rs::delivery_input", 1),
        ("src/management/v2/input_ledger_owner.rs::new_input", 1),
        ("src/management/v2/input_ledger_bus.rs::new_input", 1),
        ("src/management/v2/input_ledger_bus.rs::tell_refused", 1),
    ]
    .into_iter()
    .map(|(name, count)| (name.to_string(), count))
    .collect();
    assert_eq!(
        inventory.constructors, expected,
        "register and review a new producer before admitting it"
    );
    // Frozen protocol set is independent of the production enum's ALL table.
    use cutex::management::v2::input_ledger::{InputKind, Surface};
    assert_eq!(
        InputKind::ALL
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>(),
        [
            "owner",
            "agentMessage",
            "peerMessage",
            "alarm",
            "systemNotice",
            "delegationRequest",
            "delegationResult",
            "delegationQuestion",
            "delegationStatus"
        ]
    );
    assert_eq!(
        Surface::ALL.iter().map(|v| v.as_str()).collect::<Vec<_>>(),
        ["timeline", "nextUp", "pendingNotes", "none"]
    );
    let controls: BTreeSet<_> = [
        "src/agent_bus/claude_task.rs::send_request",
        "src/agent_bus/client.rs::agent_message_send_request",
        "src/agent_bus/client.rs::peer_tell_send_request",
        "src/agent_bus/queue.rs::enqueue_agent_bus_message_once_with_id",
        "src/agent_bus/queue.rs::queued_agent_messages",
        "src/agent_bus/server.rs::handle_agent_bus_request",
        "src/cli_app/agent_bus_server.rs::send_agent_management_notice_response",
        "src/cli_app/agent_bus_server.rs::send_agent_management_system_message_response",
        "src/cli_app/agent_bus_server.rs::send_director_address_response",
        "src/cli_app/agent_bus_server.rs::send_payload_response_with_projection",
        "src/cli_app/agent_bus_server.rs::submit_job_service_completion_inner",
        "src/cli_app/agent_name_notices.rs::drain",
        "src/management/v2/agent_bus_state.rs::analyze_backlog",
        "src/management/v2/agent_bus_state.rs::downgrade_pending_peer_wakes",
        "src/management/v2/agent_bus_state.rs::from",
        "src/management/v2/agent_bus_state.rs::stuck_view",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        inventory.controls, controls,
        "register canonical producer/control or repair provenance first"
    );
    assert_eq!(
        inventory.includes,
        BTreeSet::from(["src/cli_app/automatic_flush.rs".to_string()]),
        "review and register new production includes"
    );
    let factories: BTreeSet<_> = [
        "src/asks/ledger.rs::backfill_outcome_unknown",
        "src/asks/ledger.rs::delivery_job",
        "src/asks/ledger.rs::lease_release",
        "src/asks/ledger.rs::request_job",
        "src/asks/ledger.rs::results_jobs",
        "src/management/v2/agent_bus_state.rs::ledger_input",
        "src/management/v2/input_ledger_bus.rs::tell_refused",
        "src/management/v2/input_ledger_owner.rs::native_accepted",
        "src/management/v2/input_ledger_owner.rs::native_sending",
        "src/management/v2/input_ledger_owner.rs::native_unknown",
        "src/management/v2/input_ledger_owner.rs::route_accepted",
        "src/management/v2/input_ledger_owner.rs::write_submit_outcome",
        "src/management/v2/input_ledger_settle.rs::rebuild_owner_source",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(
        inventory.factory_calls, factories,
        "register and audit a new admission or rebuild path"
    );
}

#[test]
fn nextup_gate_does_not_ignore_production_after_a_test_module_or_a_type_alias() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let mut inventory = Inventory {
        root: root.clone(),
        file: root.join("src/probe.rs"),
        module_dir: root.join("src"),
        function: String::new(),
        files: Default::default(),
        constructors: Default::default(),
        controls: Default::default(),
        factory_calls: Default::default(),
        includes: Default::default(),
        aliases: BTreeSet::from(["NewInput".into()]),
    };
    let tree=syn::parse_file("#[cfg(test)] mod tests { fn fixture(){NewInput{}} } use crate::NewInput as Alias; fn new_business(){ Alias{} } fn another(){NewInput{}} ").unwrap();
    let mut aliases = AliasCollector::default();
    aliases.visit_file(&tree);
    inventory.aliases.extend(aliases.0);
    inventory.visit_file(&tree);
    assert_eq!(
        inventory
            .constructors
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["src/probe.rs::another", "src/probe.rs::new_business"]
    );
    assert!(!inventory
        .constructors
        .keys()
        .any(|site| site.ends_with("fixture")));
    assert!(token_mentions_input(
        "NewInput { kind: kind }".parse().unwrap()
    ));
}
