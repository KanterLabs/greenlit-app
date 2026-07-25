//! Table/fake-driven tests for the engine's pure execution semantics: shell
//! resolution, env layering, command-file parsing, and log commands +
//! masking. The step outcome model and deferred-value resolution live in
//! `execution_step_model.rs`.
//!
//! No container is involved: the container boundary is faked by handing the
//! engine raw step results (exit codes, command-file text, output lines).

use indexmap::IndexMap;

use greenlit_engine::execution::command_files::{
    Assignment, EnvFileError, accept_step_summary, parse_env_file, parse_path_file,
};
use greenlit_engine::execution::env::{EnvLayers, RunnerEnv, apply_path_additions, layer_step_env};
use greenlit_engine::execution::log_commands::{
    Annotation, LogLine, MASK_TOKEN, Masker, parse_line,
};
use greenlit_engine::execution::shell::{ShellError, ShellSelection, resolve_shell};

fn map(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

// --- shell resolution ------------------------------------------------------

#[test]
fn default_shell_is_bash_dash_e_on_ubuntu_with_bash() {
    let sel = ShellSelection {
        step_shell: None,
        default_shell: None,
        in_container: false,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/tmp/s.sh").unwrap();
    assert_eq!(inv.program, "bash");
    assert_eq!(inv.args, vec!["-e", "/tmp/s.sh"]);
}

#[test]
fn default_shell_falls_back_to_sh_without_bash() {
    let sel = ShellSelection {
        step_shell: None,
        default_shell: None,
        in_container: false,
        bash_available: false,
    };
    let inv = resolve_shell(sel, "/tmp/s.sh").unwrap();
    assert_eq!(inv.program, "sh");
    assert_eq!(inv.args, vec!["-e", "/tmp/s.sh"]);
}

#[test]
fn container_job_defaults_to_sh_even_with_bash() {
    let sel = ShellSelection {
        step_shell: None,
        default_shell: None,
        in_container: true,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/s").unwrap();
    assert_eq!(inv.program, "sh");
    assert_eq!(inv.args, vec!["-e", "/s"]);
}

#[test]
fn explicit_bash_uses_pipefail_template() {
    let sel = ShellSelection {
        step_shell: Some("bash"),
        default_shell: None,
        in_container: false,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/s").unwrap();
    assert_eq!(inv.program, "bash");
    assert_eq!(
        inv.args,
        vec!["--noprofile", "--norc", "-eo", "pipefail", "/s"]
    );
}

#[test]
fn explicit_python_and_sh_keywords() {
    let py = resolve_shell(
        ShellSelection {
            step_shell: Some("python"),
            default_shell: None,
            in_container: false,
            bash_available: true,
        },
        "/s",
    )
    .unwrap();
    assert_eq!(py.program, "python");
    assert_eq!(py.args, vec!["/s"]);
}

#[test]
fn defaults_run_shell_applies_when_step_has_none() {
    let sel = ShellSelection {
        step_shell: None,
        default_shell: Some("sh"),
        in_container: false,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/s").unwrap();
    assert_eq!(inv.program, "sh");
}

#[test]
fn step_shell_overrides_defaults_run_shell() {
    let sel = ShellSelection {
        step_shell: Some("bash"),
        default_shell: Some("sh"),
        in_container: false,
        bash_available: true,
    };
    assert_eq!(resolve_shell(sel, "/s").unwrap().program, "bash");
}

#[test]
fn custom_shell_template_substitutes_placeholder() {
    let sel = ShellSelection {
        step_shell: Some("perl -e {0} --flag"),
        default_shell: None,
        in_container: false,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/s.pl").unwrap();
    assert_eq!(inv.program, "perl");
    assert_eq!(inv.args, vec!["-e", "/s.pl", "--flag"]);
}

#[test]
fn custom_shell_without_placeholder_appends_script_path() {
    let sel = ShellSelection {
        step_shell: Some("ruby"),
        default_shell: None,
        in_container: false,
        bash_available: true,
    };
    let inv = resolve_shell(sel, "/s.rb").unwrap();
    assert_eq!(inv.program, "ruby");
    assert_eq!(inv.args, vec!["/s.rb"]);
}

#[test]
fn windows_and_pwsh_shells_are_rejected() {
    for shell in ["cmd", "powershell", "pwsh"] {
        let sel = ShellSelection {
            step_shell: Some(shell),
            default_shell: None,
            in_container: false,
            bash_available: true,
        };
        assert!(matches!(
            resolve_shell(sel, "/s"),
            Err(ShellError::UnsupportedShell { .. })
        ));
    }
}

// --- env layering ----------------------------------------------------------

#[test]
fn env_layers_most_specific_wins() {
    let base = map(&[("A", "base"), ("PATH", "/usr/bin")]);
    let workflow = map(&[("A", "workflow"), ("B", "workflow")]);
    let job = map(&[("B", "job"), ("C", "job")]);
    let accumulated = map(&[("C", "github_env"), ("D", "github_env")]);
    let step = map(&[("D", "step")]);
    let env = layer_step_env(EnvLayers {
        base: &base,
        workflow: &workflow,
        job: &job,
        accumulated: &accumulated,
        step: &step,
    });
    assert_eq!(env.get("A").unwrap(), "workflow");
    assert_eq!(env.get("B").unwrap(), "job");
    // GITHUB_ENV accumulation overrides workflow/job for later steps.
    assert_eq!(env.get("C").unwrap(), "github_env");
    // Step env is the most specific layer.
    assert_eq!(env.get("D").unwrap(), "step");
    assert_eq!(env.get("PATH").unwrap(), "/usr/bin");
}

#[test]
fn github_path_additions_prepend_to_path() {
    let mut env = map(&[("PATH", "/usr/bin:/bin")]);
    apply_path_additions(
        &mut env,
        &["/opt/tool/bin".to_string(), "/opt/other".to_string()],
    );
    assert_eq!(
        env.get("PATH").unwrap(),
        "/opt/tool/bin:/opt/other:/usr/bin:/bin"
    );
}

#[test]
fn runner_env_sets_documented_defaults() {
    let env = RunnerEnv {
        repository: "octo/repo".to_string(),
        sha: "abc123".to_string(),
        event_name: "push".to_string(),
        ..Default::default()
    }
    .into_map();
    assert_eq!(env.get("CI").unwrap(), "true");
    assert_eq!(env.get("GITHUB_ACTIONS").unwrap(), "true");
    assert_eq!(env.get("GITHUB_REPOSITORY").unwrap(), "octo/repo");
    assert_eq!(env.get("GITHUB_SHA").unwrap(), "abc123");
    assert_eq!(env.get("GITHUB_EVENT_NAME").unwrap(), "push");
    assert_eq!(env.get("RUNNER_OS").unwrap(), "Linux");
    assert_eq!(env.get("RUNNER_ARCH").unwrap(), "X64");
    assert_eq!(env.get("GITHUB_SERVER_URL").unwrap(), "https://github.com");
}

// --- command files ---------------------------------------------------------

#[test]
fn env_file_parses_simple_and_heredoc() {
    let contents = "NAME=value\nMULTI<<EOF\nline1\nline2\nEOF\nLAST=z\n";
    let parsed = parse_env_file(contents).unwrap();
    assert_eq!(
        parsed,
        vec![
            Assignment {
                name: "NAME".to_string(),
                value: "value".to_string()
            },
            Assignment {
                name: "MULTI".to_string(),
                value: "line1\nline2".to_string()
            },
            Assignment {
                name: "LAST".to_string(),
                value: "z".to_string()
            },
        ]
    );
}

#[test]
fn env_file_value_with_angle_brackets_is_not_heredoc() {
    let parsed = parse_env_file("NAME=a<<b\n").unwrap();
    assert_eq!(parsed[0].value, "a<<b");
}

#[test]
fn env_file_rejects_bad_lines_and_unterminated_heredoc() {
    assert!(matches!(
        parse_env_file("bogus line\n"),
        Err(EnvFileError::InvalidLine { .. })
    ));
    assert!(matches!(
        parse_env_file("X<<EOF\nbody\n"),
        Err(EnvFileError::UnterminatedHeredoc { .. })
    ));
    assert!(matches!(
        parse_env_file("X<<\nbody\n"),
        Err(EnvFileError::EmptyDelimiter { .. })
    ));
}

#[test]
fn path_file_and_summary_limits() {
    assert_eq!(
        parse_path_file("/a\n\n/b\n"),
        vec!["/a".to_string(), "/b".to_string()]
    );
    assert_eq!(accept_step_summary("small"), Some("small"));
    let big = "x".repeat(1024 * 1024 + 1);
    assert_eq!(accept_step_summary(&big), None);
}

// --- log commands + masking ------------------------------------------------

#[test]
fn parses_grouping_and_annotations() {
    assert_eq!(
        parse_line("::group::Build"),
        LogLine::StartGroup("Build".to_string())
    );
    assert_eq!(parse_line("::endgroup::"), LogLine::EndGroup);
    assert_eq!(
        parse_line("::error file=a.rs,line=3::boom"),
        LogLine::Error(Annotation {
            parameters: "file=a.rs,line=3".to_string(),
            message: "boom".to_string(),
        })
    );
    assert_eq!(
        parse_line("::warning::careful"),
        LogLine::Warning(Annotation {
            parameters: String::new(),
            message: "careful".to_string()
        })
    );
    assert_eq!(
        parse_line("plain text"),
        LogLine::Output("plain text".to_string())
    );
}

#[test]
fn annotation_message_is_percent_decoded() {
    let LogLine::Notice(a) = parse_line("::notice::line1%0Aline2%25") else {
        panic!("expected notice");
    };
    assert_eq!(a.message, "line1\nline2%");
}

#[test]
fn masker_redacts_registered_values_immediately() {
    let mut masker = Masker::new();
    masker.add("s3cr3t+/ value");
    masker.add("   "); // whitespace-only ignored
    assert!(!masker.is_empty());
    assert_eq!(
        masker.apply("token is s3cr3t+/ value here"),
        format!("token is {MASK_TOKEN} here")
    );
    assert_eq!(
        masker.apply("standard czNjcjN0Ky8gdmFsdWU="),
        format!("standard {MASK_TOKEN}")
    );
    assert_eq!(
        masker.apply("url czNjcjN0Ky8gdmFsdWU"),
        format!("url {MASK_TOKEN}")
    );
    assert_eq!(
        masker.apply("percent s3cr3t%2B%2F%20value"),
        format!("percent {MASK_TOKEN}")
    );
    assert_eq!(masker.apply("nothing"), "nothing");
}

#[test]
fn masker_handles_multiline_secret_lines() {
    let mut masker = Masker::new();
    masker.add("line-a\nline-b");
    assert_eq!(masker.apply("echo line-a"), format!("echo {MASK_TOKEN}"));
    assert_eq!(masker.apply("line-a\nline-b"), MASK_TOKEN);
}
