//! Small, reusable pieces of the `extract` command contract.

/// Match the compatibility environment-variable convention used by hooks and
/// the upstream CLI. Unknown values are deliberately false rather than merely
/// checking whether the variable exists.
pub fn truthy(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

/// An explicit flag wins, while both the native and legacy environment names
/// remain supported for scripted migrations.
pub fn force_enabled(
    explicit: bool,
    graphoxide_force: Option<&str>,
    graphify_force: Option<&str>,
) -> bool {
    explicit || truthy(graphoxide_force) || truthy(graphify_force)
}
