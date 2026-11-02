//! A worker panic must terminate the public builder, never strand its ordered reader.

use agent_ide::lang::*;
use agent_ide::project::structure::{StructureOptions, build_structure};
use async_lsp::lsp_types as lsp;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Deliberately fails source enrichment while unrelated trait methods use the bundled adapter.
struct FailingSupport;
static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "worker_failure",
    display_name: "Worker failure",
    extensions: &["crash"],
    card_manifest: None,
    home_tool_dirs: &[],
    support: &FailingSupport,
    checks: None,
    server: None,
    names: None,
};
impl LanguageSupport for FailingSupport {
    /// Identifies this isolated process's injected adapter.
    fn language(&self) -> Language {
        Language::of(&DESCRIPTOR)
    }
    /// No project discovery is needed for a source-only collection.
    fn detect(&self, _: &Path) -> Option<LanguageProject> {
        None
    }
    /// Delegates an unused semantic operation without introducing another implementation.
    fn normalize(&self, file: &Path, text: &str, symbols: Vec<lsp::DocumentSymbol>) -> Outline {
        agent_ide_lang_rust::LANGUAGE
            .support()
            .normalize(file, text, symbols)
    }
    /// Delegates an unused insertion operation.
    fn insert_site(
        &self,
        source: &str,
        outline: &Outline,
        anchor: &SymbolPath,
        where_: InsertWhere,
    ) -> Result<InsertSite, LangError> {
        agent_ide_lang_rust::LANGUAGE
            .support()
            .insert_site(source, outline, anchor, where_)
    }
    /// Collection does not select tests.
    fn is_test_file(&self, _: &Path) -> bool {
        false
    }
    /// Delegates an unused test-selection operation.
    fn test_selection(
        &self,
        project: &LanguageProject,
        target: &TestTarget,
    ) -> Result<TestSelection, LangError> {
        agent_ide_lang_rust::LANGUAGE
            .support()
            .test_selection(project, target)
    }
    /// Delegates an unused runner-output operation.
    fn parse_test_output(&self, stdout: &str, stderr: &str) -> TestReport {
        agent_ide_lang_rust::LANGUAGE
            .support()
            .parse_test_output(stdout, stderr)
    }
    /// Collection never starts formatters.
    fn format_command(&self, _: &LanguageProject, _: &Path) -> Option<Vec<String>> {
        None
    }
    /// Collection never starts stdin formatters.
    fn format_stdin_command(&self, _: &LanguageProject, _: &Path) -> Option<Vec<String>> {
        None
    }
    /// Every admitted source job panics so the coordinator loses all workers.
    fn source_details(
        &self,
        _: &Path,
        _: &str,
        _: bool,
        _: bool,
    ) -> (Option<Vec<LineRange>>, Option<Outline>) {
        panic!("injected source parser failure")
    }
}

/// Owns only the temporary files for this crash-path subprocess.
struct Fixture(PathBuf);
impl Drop for Fixture {
    /// Preserves unrelated temporary data even if the timeout assertion fails.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Isolates registry and deliberate thread panics; a deadline detects an actual queue deadlock.
#[test]
fn structure_worker_panic_returns_error_without_hanging() {
    const MARKER: &str = "STRUCTURE_FAILURE_CHILD";
    if let Some(root) = std::env::var_os(MARKER) {
        assert!(agent_ide::lang::install(&[Language::of(&DESCRIPTOR)]));
        let result = build_structure(
            Path::new(&root),
            &StructureOptions {
                respect_gitignore: false,
                include_skeleton: true,
                workers: 2,
                ..Default::default()
            },
        );
        assert!(
            result.is_err(),
            "worker failure cannot yield a successful partial tree"
        );
        return;
    }
    let fixture =
        Fixture(std::env::temp_dir().join(format!("structure-panic-{}", std::process::id())));
    std::fs::create_dir(&fixture.0).unwrap();
    for n in 0..16 {
        std::fs::write(fixture.0.join(format!("{n:02}.crash")), "source").unwrap();
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "structure_worker_panic_returns_error_without_hanging",
            "--nocapture",
        ])
        .env(MARKER, &fixture.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            break;
        }
        if started.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            let _ = child.wait();
            panic!("worker failure stranded the source reader for five seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
