//! `ch --list-plugins`.

use std::fmt::Write as _;

use ch_kernel::Registry;

/// Each plugin with its version, whether it is enabled and why, its description, and
/// everything it registered.
pub fn format_plugin_list(registry: &Registry) -> String {
    let mut out = String::new();
    for status in &registry.plugins {
        let m = &status.manifest;
        let state = if status.enabled {
            "enabled"
        } else {
            "disabled"
        };
        let _ = writeln!(out, "{} {}  {state} ({})", m.id, m.version, status.reason);
        if !m.description.is_empty() {
            let _ = writeln!(out, "    {}", m.description);
        }
        if !m.requires.is_empty() {
            let _ = writeln!(out, "    requires: {}", m.requires.join(", "));
        }
        for (label, items) in registered(registry, &m.id) {
            if !items.is_empty() {
                let _ = writeln!(out, "    {label}: {}", items.join(", "));
            }
        }
    }
    let services = registry.services.names();
    if !services.is_empty() {
        let _ = writeln!(out, "services: {}", services.join(", "));
    }
    out
}

/// `(label, names)` of what plugin `id` registered.
fn registered(r: &Registry, id: &str) -> Vec<(&'static str, Vec<String>)> {
    fn names<T: ?Sized>(
        items: &[(String, std::sync::Arc<T>)],
        id: &str,
        name: impl Fn(&T) -> String,
    ) -> Vec<String> {
        items
            .iter()
            .filter(|(p, _)| p == id)
            .map(|(_, x)| name(x))
            .collect()
    }
    vec![
        ("providers", names(&r.providers, id, |p| p.id().to_string())),
        ("tools", names(&r.tools, id, |t| t.spec().name)),
        (
            "hooks",
            names(&r.hooks, id, |h| {
                let points: Vec<&str> = h.points().iter().map(|p| p.as_str()).collect();
                format!("{} [{}]", h.id(), points.join(" "))
            }),
        ),
        ("prompts", names(&r.prompts, id, |p| p.id().to_string())),
        (
            "context strategies",
            names(&r.contexts, id, |c| c.id().to_string()),
        ),
        (
            "commands",
            names(&r.commands, id, |c| format!("/{}", c.name())),
        ),
        ("event sinks", names(&r.sinks, id, |s| s.id().to_string())),
        ("executors", names(&r.executors, id, |e| e.id().to_string())),
        ("approvers", names(&r.approvers, id, |a| a.id().to_string())),
        ("frontends", names(&r.frontends, id, |f| f.id().to_string())),
    ]
}
