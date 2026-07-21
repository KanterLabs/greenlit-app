//! Human rendering for reusable workflow/job/leg configuration sections.

use std::io::Write;

use greenlit_engine::{
    ContainerPlan, EnvValue, Evaluation, JobOutputsPlan, PermissionsPlan, PlannedOutput,
    PlannedValue, RunDefaultsPlan,
};

use super::format::{format_defer_reasons, format_planned_string};

pub(super) fn render_env<'a>(
    label: &str,
    entries: impl IntoIterator<Item = (&'a String, &'a EnvValue)>,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    let mut entries = entries.into_iter().peekable();
    if entries.peek().is_none() {
        return writeln!(out, "{indent}{label}: (none)");
    }
    writeln!(out, "{indent}{label}:")?;
    for (name, value) in entries {
        writeln!(out, "{indent}  {name}: {}", format_planned_string(value))?;
    }
    Ok(())
}

pub(super) fn render_defaults(
    defaults: &RunDefaultsPlan,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    if defaults.shell.is_none() && defaults.working_directory.is_none() {
        return writeln!(out, "{indent}defaults.run: (none)");
    }
    writeln!(out, "{indent}defaults.run:")?;
    match &defaults.shell {
        Some(shell) => writeln!(out, "{indent}  shell: {}", format_planned_string(shell))?,
        None => writeln!(out, "{indent}  shell: (default)")?,
    }
    match &defaults.working_directory {
        Some(directory) => writeln!(
            out,
            "{indent}  working-directory: {}",
            format_planned_string(directory)
        )?,
        None => writeln!(out, "{indent}  working-directory: (default)")?,
    }
    Ok(())
}

pub(super) fn render_permissions(
    permissions: Option<&PermissionsPlan>,
    out: &mut impl Write,
) -> std::io::Result<()> {
    match permissions {
        None => writeln!(out, "permissions: (GitHub default)"),
        Some(PermissionsPlan::ReadAll) => writeln!(out, "permissions: read-all"),
        Some(PermissionsPlan::WriteAll) => writeln!(out, "permissions: write-all"),
        Some(permissions @ PermissionsPlan::Scoped { .. }) => {
            let serialized = serde_json::to_string(permissions).map_err(std::io::Error::other)?;
            writeln!(out, "permissions: {serialized}")
        }
    }
}

pub(super) fn render_container(
    label: &str,
    container: Option<&ContainerPlan>,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    let Some(container) = container else {
        return writeln!(out, "{indent}{label}: (none)");
    };
    writeln!(out, "{indent}{label}:")?;
    render_container_fields(container, out, &format!("{indent}  "))
}

pub(super) fn render_services<'a>(
    services: impl IntoIterator<Item = (&'a String, &'a ContainerPlan)>,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    let mut services = services.into_iter().peekable();
    if services.peek().is_none() {
        return writeln!(out, "{indent}services: (none)");
    }
    writeln!(out, "{indent}services:")?;
    for (name, service) in services {
        writeln!(out, "{indent}  {name}:")?;
        render_container_fields(service, out, &format!("{indent}    "))?;
    }
    Ok(())
}

fn render_container_fields(
    container: &ContainerPlan,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    writeln!(
        out,
        "{indent}image: {}",
        format_planned_string(&container.image)
    )?;
    match &container.credentials {
        None => writeln!(out, "{indent}credentials: (none)")?,
        Some(credentials) => {
            writeln!(out, "{indent}credentials:")?;
            match &credentials.username {
                Some(username) => writeln!(
                    out,
                    "{indent}  username: {}",
                    format_planned_string(username)
                )?,
                None => writeln!(out, "{indent}  username: (none)")?,
            }
            match &credentials.password {
                Some(password) => {
                    writeln!(out, "{indent}  password: {}", format_password(password))?
                }
                None => writeln!(out, "{indent}  password: (none)")?,
            }
        }
    }
    render_env("env", &container.env, out, indent)?;
    render_values("ports", &container.ports, out, indent)?;
    render_values("volumes", &container.volumes, out, indent)?;
    match &container.options {
        Some(options) => writeln!(out, "{indent}options: {}", format_planned_string(options))?,
        None => writeln!(out, "{indent}options: (none)")?,
    }
    Ok(())
}

fn format_password(value: &EnvValue) -> String {
    match &value.evaluation {
        Evaluation::Static(_) => "static([masked])".to_string(),
        Evaluation::Deferred(deferred) => format!(
            "deferred <- {} (defers on: {})",
            deferred.residual_text,
            format_defer_reasons(&deferred.defers_on)
        ),
    }
}

fn render_values(
    label: &str,
    values: &[EnvValue],
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    if values.is_empty() {
        return writeln!(out, "{indent}{label}: (none)");
    }
    writeln!(out, "{indent}{label}:")?;
    for value in values {
        writeln!(out, "{indent}  - {}", format_planned_string(value))?;
    }
    Ok(())
}

pub(super) fn render_outputs(
    outputs: &JobOutputsPlan,
    out: &mut impl Write,
    indent: &str,
) -> std::io::Result<()> {
    if outputs.entries.is_empty() {
        return writeln!(out, "{indent}outputs: (none)");
    }
    writeln!(out, "{indent}outputs:")?;
    for (name, output) in &outputs.entries {
        writeln!(out, "{indent}  {name}: {}", format_output(output))?;
    }
    Ok(())
}

fn format_output(output: &PlannedOutput) -> String {
    match &output.value {
        PlannedValue::Static(value) => format!("static({value:?}) <- {}", output.source),
        PlannedValue::Deferred(deferred) => format!(
            "deferred <- {} (defers on: {})",
            deferred.residual_text,
            format_defer_reasons(&deferred.defers_on)
        ),
    }
}
