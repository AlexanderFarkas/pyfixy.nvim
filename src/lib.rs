use anyhow::Result;
use lsp_types::{
    CodeAction, CodeActionKind, CompletionItem, CompletionItemKind, Diagnostic, DiagnosticSeverity,
    Location, Position, Range, TextEdit, Url, WorkspaceEdit,
};
use ruff_python_ast::Decorator;
use ruff_python_ast::{
    Expr, ExprCall, ExprName, ExprStringLiteral, Parameter, Stmt, StmtFunctionDef,
};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextSize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixture {
    pub name: String,
    pub path: PathBuf,
    pub visibility_path: PathBuf,
    pub range: Range,
    pub name_range: Range,
    pub return_annotation: Option<FixtureAnnotation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureAnnotation {
    pub text: String,
    pub imports: Vec<ImportRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImportRequirement {
    pub module: String,
    pub name: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationDiagnosticKind {
    Missing,
    Mismatched,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureAnnotationDiagnostic {
    pub kind: AnnotationDiagnosticKind,
    pub fixture_name: String,
    pub fixture_annotation: String,
    pub range: Range,
    pub edit_range: Range,
}

#[derive(Debug, Clone, Copy)]
pub struct DiagnosticConfig {
    pub missing_annotation: DiagnosticSeverity,
    pub mismatched_annotation: DiagnosticSeverity,
}

impl Default for DiagnosticConfig {
    fn default() -> Self {
        Self {
            missing_annotation: DiagnosticSeverity::HINT,
            mismatched_annotation: DiagnosticSeverity::ERROR,
        }
    }
}

#[derive(Debug, Default)]
pub struct FixtureIndex {
    root: PathBuf,
    fixtures: Vec<Fixture>,
}

impl FixtureIndex {
    pub fn build(root: &Path) -> Result<Self> {
        let root = normalize_path(root);
        let mut fixtures = Vec::new();
        collect_fixtures(&root, &root, &mut fixtures)?;
        expand_conftest_reexports(&root, &mut fixtures)?;
        Ok(Self { root, fixtures })
    }

    pub fn completions(&self, file: &Path, position: Position) -> Result<Vec<CompletionItem>> {
        let file = normalize_path(file);
        let text = fs::read_to_string(&file)?;
        self.completions_for_text(&file, &text, position)
    }

    pub fn completions_for_text(
        &self,
        file: &Path,
        text: &str,
        position: Position,
    ) -> Result<Vec<CompletionItem>> {
        let file = normalize_path(file);
        if !is_in_function_params(text, position) {
            return Ok(Vec::new());
        }
        Ok(self
            .visible_fixtures_for_text(&file, text)
            .into_iter()
            .map(|f| {
                let mut item = CompletionItem {
                    label: f.name.clone(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some(format!("pytest fixture ({})", f.path.display())),
                    ..Default::default()
                };
                if let Some(annotation) = &f.return_annotation {
                    if is_in_parameter_annotation_context(text, position) {
                        item.insert_text = Some(f.name.clone());
                    } else {
                        item.insert_text = Some(format!("{}: {}", f.name, annotation.text));
                        let edits = import_text_edits(text, annotation);
                        if !edits.is_empty() {
                            item.additional_text_edits = Some(edits);
                        }
                    }
                }
                item
            })
            .collect())
    }

    pub fn definition(&self, file: &Path, position: Position) -> Result<Option<Location>> {
        let file = normalize_path(file);
        let text = fs::read_to_string(&file)?;
        let Some(name) = identifier_at_position(&text, position) else {
            return Ok(None);
        };
        if !is_in_function_params(&text, position) {
            return Ok(None);
        }
        let Some(fixture) = self
            .visible_fixtures(&file)
            .into_iter()
            .find(|f| f.name == name)
        else {
            return Ok(None);
        };
        Ok(Some(Location {
            uri: Url::from_file_path(&fixture.path).unwrap(),
            range: fixture.range,
        }))
    }

    pub fn diagnostics_for_text(
        &self,
        file: &Path,
        text: &str,
        config: DiagnosticConfig,
    ) -> Result<Vec<Diagnostic>> {
        Ok(self
            .annotation_diagnostics_for_text(file, text)?
            .into_iter()
            .map(|d| Diagnostic {
                range: d.range,
                severity: Some(match d.kind {
                    AnnotationDiagnosticKind::Missing => config.missing_annotation,
                    AnnotationDiagnosticKind::Mismatched => config.mismatched_annotation,
                }),
                source: Some("pyfixy".into()),
                message: match d.kind {
                    AnnotationDiagnosticKind::Missing => format!(
                        "Fixture `{}` returns `{}`; parameter is missing an annotation",
                        d.fixture_name, d.fixture_annotation
                    ),
                    AnnotationDiagnosticKind::Mismatched => format!(
                        "Fixture `{}` returns `{}`, but parameter has a different annotation",
                        d.fixture_name, d.fixture_annotation
                    ),
                },
                ..Default::default()
            })
            .collect())
    }

    pub fn annotation_diagnostics_for_text(
        &self,
        file: &Path,
        text: &str,
    ) -> Result<Vec<FixtureAnnotationDiagnostic>> {
        let file = normalize_path(file);
        let visible: HashMap<String, &Fixture> = self
            .visible_fixtures_for_text(&file, text)
            .into_iter()
            .map(|fixture| (fixture.name.clone(), fixture))
            .collect();
        Ok(parameter_annotation_infos(text)
            .into_iter()
            .filter_map(|param| {
                let fixture = visible.get(&param.name)?;
                let fixture_annotation = fixture_value_annotation(fixture)?;
                match param.annotation {
                    std::option::Option::None => Some(FixtureAnnotationDiagnostic {
                        kind: AnnotationDiagnosticKind::Missing,
                        fixture_name: param.name,
                        fixture_annotation: fixture_annotation.text,
                        range: param.name_range,
                        edit_range: param.name_range,
                    }),
                    Some((text_annotation, range))
                        if !annotations_equivalent(
                            &text_annotation,
                            &fixture_annotation.text,
                            &file,
                            text,
                            fixture,
                        ) =>
                    {
                        Some(FixtureAnnotationDiagnostic {
                            kind: AnnotationDiagnosticKind::Mismatched,
                            fixture_name: param.name,
                            fixture_annotation: fixture_annotation.text,
                            range,
                            edit_range: range,
                        })
                    }
                    _ => None,
                }
            })
            .collect())
    }

    pub fn code_actions_for_text(
        &self,
        file: &Path,
        text: &str,
        uri: Url,
    ) -> Result<Vec<CodeAction>> {
        self.code_actions_for_text_range(file, text, uri, None)
    }

    pub fn code_actions_for_text_range(
        &self,
        file: &Path,
        text: &str,
        uri: Url,
        requested_range: Option<Range>,
    ) -> Result<Vec<CodeAction>> {
        let file = normalize_path(file);
        let diagnostics = self.annotation_diagnostics_for_text(&file, text)?;
        let visible: HashMap<String, &Fixture> = self
            .visible_fixtures_for_text(&file, text)
            .into_iter()
            .map(|fixture| (fixture.name.clone(), fixture))
            .collect();
        let mut actions = Vec::new();
        for d in diagnostics.iter().filter(|d| {
            requested_range
                .map(|range| ranges_intersect_or_touch_cursor(d.range, range))
                .unwrap_or(true)
        }) {
            let Some(fixture) = visible.get(&d.fixture_name) else {
                continue;
            };
            let edits = edits_for_annotation(text, d, fixture);
            actions.push(CodeAction {
                title: "Add fixture type annotation".into(),
                kind: Some(CodeActionKind::QUICKFIX),
                edit: Some(workspace_edit(uri.clone(), coalesce_text_edits(edits))),
                ..Default::default()
            });
        }
        if !diagnostics.is_empty() {
            let mut edits = Vec::new();
            for d in &diagnostics {
                if let Some(fixture) = visible.get(&d.fixture_name) {
                    edits.extend(edits_for_annotation(text, d, fixture));
                }
            }
            actions.push(CodeAction {
                title: "Add fixture type annotations in file".into(),
                kind: Some(CodeActionKind::SOURCE),
                edit: Some(workspace_edit(uri, coalesce_text_edits(edits))),
                ..Default::default()
            });
        }
        Ok(actions)
    }

    pub fn references(&self, file: &Path, position: Position) -> Result<Vec<Location>> {
        let file = normalize_path(file);
        let Some(name) = self.fixture_name_at(&file, position)? else {
            return Ok(Vec::new());
        };

        let mut locations = Vec::new();
        let mut files = Vec::new();
        collect_python_files(&self.root, &mut files)?;
        for candidate in files {
            if !self
                .visible_fixtures(&candidate)
                .into_iter()
                .any(|fixture| fixture.name == name)
            {
                continue;
            }
            let text = fs::read_to_string(&candidate)?;
            for range in parameter_name_ranges(&text, &name) {
                locations.push(Location {
                    uri: Url::from_file_path(&candidate).unwrap(),
                    range,
                });
            }
        }
        locations.sort_by(|a, b| {
            a.uri
                .cmp(&b.uri)
                .then_with(|| a.range.start.line.cmp(&b.range.start.line))
                .then_with(|| a.range.start.character.cmp(&b.range.start.character))
        });
        Ok(locations)
    }

    fn fixture_name_at(&self, file: &Path, position: Position) -> Result<Option<String>> {
        let file = normalize_path(file);
        let text = fs::read_to_string(&file)?;
        if is_in_function_params(&text, position) {
            let Some(name) = identifier_at_position(&text, position) else {
                return Ok(None);
            };
            return Ok(self
                .visible_fixtures(&file)
                .into_iter()
                .any(|fixture| fixture.name == name)
                .then_some(name));
        }

        Ok(self
            .fixtures
            .iter()
            .find(|fixture| fixture.path == file && position_in_range(position, fixture.name_range))
            .map(|fixture| fixture.name.clone()))
    }

    fn visible_fixtures(&self, file: &Path) -> Vec<&Fixture> {
        let file = normalize_path(file);
        let text = fs::read_to_string(&file).unwrap_or_default();
        self.visible_fixtures_for_text(&file, &text)
    }

    fn visible_fixtures_for_text(&self, file: &Path, text: &str) -> Vec<&Fixture> {
        let file = normalize_path(file);
        let file_dir = file.parent().unwrap_or_else(|| Path::new(""));
        let imports =
            imported_fixture_sources_from_text(&file, text, &self.root).unwrap_or_default();
        let plugin_modules = pytest_plugin_modules(&file).unwrap_or_default();
        let plugin_paths: HashSet<PathBuf> = plugin_modules
            .iter()
            .filter_map(|module| module_to_path(&self.root, module))
            .collect();

        let mut by_name: HashMap<&str, (&Fixture, usize)> = HashMap::new();
        for fixture in &self.fixtures {
            let Some(score) = fixture_visibility_score(
                fixture,
                &file,
                file_dir,
                &imports,
                &plugin_paths,
                &self.root,
            ) else {
                continue;
            };
            match by_name.get(fixture.name.as_str()) {
                Some((_, existing_score)) if *existing_score >= score => {}
                _ => {
                    by_name.insert(fixture.name.as_str(), (fixture, score));
                }
            }
        }

        let mut fixtures: Vec<_> = by_name.into_values().collect();
        fixtures.sort_by(|(a, _), (b, _)| a.name.cmp(&b.name));
        fixtures.into_iter().map(|(fixture, _)| fixture).collect()
    }
}

fn fixture_visibility_score(
    fixture: &Fixture,
    file: &Path,
    file_dir: &Path,
    imports: &HashMap<String, HashSet<PathBuf>>,
    plugin_paths: &HashSet<PathBuf>,
    root: &Path,
) -> Option<usize> {
    if fixture.visibility_path == file {
        return Some(10_000);
    }

    if imports
        .get(&fixture.name)
        .is_some_and(|paths| paths.is_empty() || paths.contains(&fixture.path))
    {
        return Some(9_000);
    }

    if plugin_paths.contains(&fixture.path) {
        return Some(8_000);
    }

    if fixture.visibility_path.file_name().and_then(|s| s.to_str()) == Some("conftest.py") {
        if let Some(dir) = fixture.visibility_path.parent() {
            if file_dir.starts_with(dir) {
                let depth = dir
                    .strip_prefix(root)
                    .map(|p| p.components().count())
                    .unwrap_or(0);
                return Some(1_000 + depth);
            }
        }
    }

    None
}

fn imported_fixture_sources_from_text(
    _file: &Path,
    text: &str,
    root: &Path,
) -> Result<HashMap<String, HashSet<PathBuf>>> {
    let Ok(parsed) = parse_module(text) else {
        return Ok(HashMap::new());
    };
    if parsed.has_invalid_syntax() {
        return Ok(HashMap::new());
    }

    let mut names: HashMap<String, HashSet<PathBuf>> = HashMap::new();
    for stmt in &parsed.syntax().body {
        if let Stmt::ImportFrom(import_from) = stmt {
            let source = import_from
                .module
                .as_ref()
                .and_then(|module| module_to_path(root, module.as_str()));
            for alias in &import_from.names {
                let visible_name = alias
                    .asname
                    .as_ref()
                    .unwrap_or(&alias.name)
                    .as_str()
                    .to_string();
                let paths = names.entry(visible_name).or_default();
                if let Some(source) = &source {
                    paths.insert(source.clone());
                }
            }
        }
    }
    Ok(names)
}

fn pytest_plugin_modules(file: &Path) -> Result<Vec<String>> {
    let mut modules = Vec::new();
    for path in pytest_plugin_config_files(file) {
        modules.extend(pytest_plugin_modules_in_file(&path)?);
    }
    Ok(modules)
}

fn pytest_plugin_config_files(file: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.push(file.to_path_buf());

    let mut dir = file.parent();
    while let Some(current) = dir {
        let conftest = current.join("conftest.py");
        if conftest.exists() {
            paths.push(conftest);
        }
        dir = current.parent();
    }

    paths
}

fn pytest_plugin_modules_in_file(file: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(file)?;
    let Ok(parsed) = parse_module(text.as_str()) else {
        return Ok(Vec::new());
    };
    if parsed.has_invalid_syntax() {
        return Ok(Vec::new());
    }

    let mut modules = Vec::new();
    for stmt in &parsed.syntax().body {
        let Stmt::Assign(assign) = stmt else {
            continue;
        };
        if assign.targets.iter().any(is_pytest_plugins_target) {
            collect_pytest_plugin_module_exprs(assign.value.as_ref(), &mut modules);
        }
    }
    Ok(modules)
}

fn is_pytest_plugins_target(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(ExprName { id, .. }) if id == "pytest_plugins")
}

fn collect_pytest_plugin_module_exprs(expr: &Expr, modules: &mut Vec<String>) {
    match expr {
        Expr::StringLiteral(ExprStringLiteral { value, .. }) => modules.push(value.to_string()),
        Expr::List(list) => {
            for elt in &list.elts {
                collect_pytest_plugin_module_exprs(elt, modules);
            }
        }
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                collect_pytest_plugin_module_exprs(elt, modules);
            }
        }
        _ => {}
    }
}

fn module_to_path(root: &Path, module: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in module.split('.') {
        path.push(part);
    }
    path.set_extension("py");
    path.exists().then_some(path)
}

#[derive(Debug)]
enum ImportSpec {
    Named {
        imported: String,
        visible: String,
        source: PathBuf,
    },
    Star {
        source: PathBuf,
    },
}

fn expand_conftest_reexports(root: &Path, fixtures: &mut Vec<Fixture>) -> Result<()> {
    let mut files = Vec::new();
    collect_python_files(root, &mut files)?;
    let conftests: Vec<_> = files
        .into_iter()
        .filter(|path| path.file_name().and_then(|name| name.to_str()) == Some("conftest.py"))
        .collect();

    let mut changed = true;
    while changed {
        changed = false;
        for conftest in &conftests {
            for spec in import_specs(conftest, root)? {
                let exports: Vec<Fixture> = match spec {
                    ImportSpec::Named {
                        imported,
                        visible,
                        source,
                    } => fixtures
                        .iter()
                        .filter(|fixture| {
                            fixture.name == imported
                                && (fixture.path == source || fixture.visibility_path == source)
                        })
                        .map(|fixture| {
                            let mut fixture = fixture.clone();
                            fixture.name = visible.clone();
                            fixture.visibility_path = conftest.clone();
                            fixture
                        })
                        .collect(),
                    ImportSpec::Star { source } => fixtures
                        .iter()
                        .filter(|fixture| {
                            fixture.visibility_path == source || fixture.path == source
                        })
                        .map(|fixture| {
                            let mut fixture = fixture.clone();
                            fixture.visibility_path = conftest.clone();
                            fixture
                        })
                        .collect(),
                };

                for export in exports {
                    let exists = fixtures.iter().any(|fixture| {
                        fixture.name == export.name
                            && fixture.path == export.path
                            && fixture.visibility_path == export.visibility_path
                    });
                    if !exists {
                        fixtures.push(export);
                        changed = true;
                    }
                }
            }
        }
    }

    Ok(())
}

fn import_specs(file: &Path, root: &Path) -> Result<Vec<ImportSpec>> {
    let text = fs::read_to_string(file)?;
    let Ok(parsed) = parse_module(text.as_str()) else {
        return Ok(Vec::new());
    };
    if parsed.has_invalid_syntax() {
        return Ok(Vec::new());
    }

    let mut specs = Vec::new();
    for stmt in &parsed.syntax().body {
        if let Stmt::ImportFrom(import_from) = stmt {
            let Some(source) = import_from
                .module
                .as_ref()
                .and_then(|module| module_to_path(root, module.as_str()))
            else {
                continue;
            };
            for alias in &import_from.names {
                if alias.name.as_str() == "*" {
                    specs.push(ImportSpec::Star {
                        source: source.clone(),
                    });
                } else {
                    specs.push(ImportSpec::Named {
                        imported: alias.name.as_str().to_string(),
                        visible: alias
                            .asname
                            .as_ref()
                            .unwrap_or(&alias.name)
                            .as_str()
                            .to_string(),
                        source: source.clone(),
                    });
                }
            }
        }
    }
    Ok(specs)
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn collect_python_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if matches!(
                name.to_str(),
                Some(".git" | ".venv" | "venv" | "__pycache__")
            ) {
                continue;
            }
            collect_python_files(&path, files)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
            files.push(path);
        }
    }
    Ok(())
}

fn collect_fixtures(root: &Path, dir: &Path, fixtures: &mut Vec<Fixture>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if matches!(
                name.to_str(),
                Some(".git" | ".venv" | "venv" | "__pycache__")
            ) {
                continue;
            }
            collect_fixtures(root, &path, fixtures)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("py") {
            let text = fs::read_to_string(&path)?;
            fixtures.extend(fixtures_in_text(&path, &text));
        }
    }
    let _ = root;
    Ok(())
}

pub fn fixtures_in_text(path: &Path, text: &str) -> Vec<Fixture> {
    let Ok(parsed) = parse_module(text) else {
        return Vec::new();
    };
    if parsed.has_invalid_syntax() {
        return Vec::new();
    }
    let mut fixtures = Vec::new();
    for stmt in &parsed.syntax().body {
        collect_stmt_fixtures(path, text, stmt, &mut fixtures);
    }
    fixtures
}

fn collect_stmt_fixtures(path: &Path, text: &str, stmt: &Stmt, fixtures: &mut Vec<Fixture>) {
    match stmt {
        Stmt::FunctionDef(func) => {
            if let Some(name) = fixture_name(func) {
                let name_range = function_name_range(text, func)
                    .unwrap_or_else(|| range_for_text_range(text, func.range()));
                fixtures.push(Fixture {
                    name,
                    path: path.to_path_buf(),
                    visibility_path: path.to_path_buf(),
                    range: range_for_text_range(text, func.range()),
                    name_range,
                    return_annotation: fixture_return_annotation(path, text, func),
                });
            }
        }
        Stmt::ClassDef(class) => {
            for stmt in &class.body {
                collect_stmt_fixtures(path, text, stmt, fixtures);
            }
        }
        _ => {}
    }
}

fn fixture_name(func: &StmtFunctionDef) -> Option<String> {
    for deco in &func.decorator_list {
        if is_fixture_decorator(deco) {
            if let Expr::Call(call) = &deco.expression {
                if let Some(name) = fixture_name_kw(call) {
                    return Some(name);
                }
            }
            return Some(func.name.to_string());
        }
    }
    None
}

fn is_fixture_decorator(decorator: &Decorator) -> bool {
    let expr = &decorator.expression;
    match expr {
        Expr::Name(ExprName { id, .. }) => id == "fixture",
        Expr::Attribute(attr) => {
            attr.attr.as_str() == "fixture"
                && matches!(attr.value.as_ref(), Expr::Name(ExprName { id, .. }) if id == "pytest")
        }
        Expr::Call(call) => is_fixture_expr(call.func.as_ref()),
        _ => false,
    }
}

fn is_fixture_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Name(ExprName { id, .. }) => id == "fixture",
        Expr::Attribute(attr) => {
            attr.attr.as_str() == "fixture"
                && matches!(attr.value.as_ref(), Expr::Name(ExprName { id, .. }) if id == "pytest")
        }
        Expr::Call(call) => is_fixture_expr(call.func.as_ref()),
        _ => false,
    }
}

fn fixture_name_kw(call: &ExprCall) -> Option<String> {
    for kw in &call.arguments.keywords {
        if kw.arg.as_ref().is_some_and(|arg| arg.as_str() == "name") {
            if let Expr::StringLiteral(ExprStringLiteral { value, .. }) = &kw.value {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn function_name_range(text: &str, func: &StmtFunctionDef) -> Option<Range> {
    let start = u32::from(func.range().start()) as usize;
    let end = u32::from(func.range().end()) as usize;
    let stmt = text.get(start..end)?;
    let name_pos = stmt.find(func.name.as_str())?;
    Some(Range {
        start: byte_to_position(text, TextSize::new((start + name_pos) as u32)),
        end: byte_to_position(
            text,
            TextSize::new((start + name_pos + func.name.as_str().len()) as u32),
        ),
    })
}

fn parameter_name_ranges(text: &str, name: &str) -> Vec<Range> {
    let Ok(parsed) = parse_module(text) else {
        return Vec::new();
    };
    if parsed.has_invalid_syntax() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    collect_parameter_name_ranges(&parsed.syntax().body, text, name, &mut ranges);
    ranges
}

fn collect_parameter_name_ranges(body: &[Stmt], text: &str, name: &str, ranges: &mut Vec<Range>) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                for parameter in func.parameters.iter() {
                    if parameter.name().as_str() == name {
                        ranges.push(range_for_text_range(text, parameter.range()));
                    }
                }
                collect_parameter_name_ranges(&func.body, text, name, ranges);
            }
            Stmt::ClassDef(class) => collect_parameter_name_ranges(&class.body, text, name, ranges),
            _ => {}
        }
    }
}

fn position_in_range(position: Position, range: Range) -> bool {
    (position.line > range.start.line
        || position.line == range.start.line && position.character >= range.start.character)
        && (position.line < range.end.line
            || position.line == range.end.line && position.character <= range.end.character)
}

fn is_in_parameter_annotation_context(text: &str, position: Position) -> bool {
    let offset = position_to_byte(text, position).min(text.len());
    let Some(before) = text.get(..offset) else {
        return false;
    };
    let Some(def_pos) = before.rfind("def ") else {
        return false;
    };
    let header = &before[def_pos..];
    let Some(open) = header.find('(') else {
        return false;
    };
    let params = &header[open + 1..];
    let colon = params.rfind(':');
    let comma = params.rfind(',');
    colon.is_some_and(|colon| comma.is_none_or(|comma| colon > comma))
}

fn is_in_function_params(text: &str, position: Position) -> bool {
    let Ok(parsed) = parse_module(text) else {
        return false;
    };
    if parsed.has_invalid_syntax() {
        return fallback_is_in_function_params(text, position);
    }

    let offset = position_to_byte(text, position);
    function_at_offset(&parsed.syntax().body, offset).is_some_and(|func| {
        let range = func.parameters.range;
        let start = u32::from(range.start()) as usize;
        let end = u32::from(range.end()) as usize;
        start <= offset && offset <= end
    })
}

fn function_at_offset<'a>(body: &'a [Stmt], offset: usize) -> Option<&'a StmtFunctionDef> {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                let range = func.range();
                let start = u32::from(range.start()) as usize;
                let end = u32::from(range.end()) as usize;
                if start <= offset && offset <= end {
                    return Some(func);
                }
            }
            Stmt::ClassDef(class) => {
                if let Some(func) = function_at_offset(&class.body, offset) {
                    return Some(func);
                }
            }
            _ => {}
        }
    }
    None
}

fn fallback_is_in_function_params(text: &str, position: Position) -> bool {
    let offset = position_to_byte(text, position);
    let Some(before) = text.get(..offset.min(text.len())) else {
        return false;
    };
    let Some(def_pos) = before.rfind("def ") else {
        return false;
    };
    let Some(after_def) = text.get(def_pos..offset.min(text.len())) else {
        return false;
    };
    if after_def.contains(':') {
        return false;
    }
    let Some(open) = after_def.find('(') else {
        return false;
    };
    after_def
        .get(open + 1..)
        .is_some_and(|params| !params.contains(')'))
}

fn identifier_at_position(text: &str, position: Position) -> Option<String> {
    let offset = position_to_byte(text, position).min(text.len());
    let bytes = text.as_bytes();
    let mut start = offset;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && is_ident(bytes[end]) {
        end += 1;
    }
    (start < end).then(|| text.get(start..end).unwrap_or_default().to_string())
}

fn is_ident(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn range_for_text_range(text: &str, range: ruff_text_size::TextRange) -> Range {
    Range {
        start: byte_to_position(text, range.start()),
        end: byte_to_position(text, range.end()),
    }
}

fn byte_to_position(text: &str, size: TextSize) -> Position {
    let target = u32::from(size) as usize;
    let mut line = 0;
    let mut col = 0;
    for (idx, ch) in text.char_indices() {
        if idx >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    Position {
        line,
        character: col,
    }
}

fn position_to_byte(text: &str, position: Position) -> usize {
    let mut line = 0u32;
    let mut col = 0u32;
    for (idx, ch) in text.char_indices() {
        if line == position.line && col >= position.character {
            return idx;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += ch.len_utf16() as u32;
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn file_url(path: impl AsRef<Path>) -> Url {
        Url::from_file_path(normalize_path(path.as_ref())).unwrap()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn discovers_pytest_fixture_decorators_and_custom_names() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tests/conftest.py");
        write(&file, "import pytest\n\n@pytest.fixture\ndef db(): pass\n\n@pytest.fixture(name=\"client\")\ndef make_client(): pass\n");
        let fixtures = fixtures_in_text(&file, &fs::read_to_string(&file).unwrap());
        let names: Vec<_> = fixtures.into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["db", "client"]);
    }

    #[test]
    fn completes_visible_conftest_fixtures_in_test_parameters() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing(\n): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "db"));
    }

    #[test]
    fn does_not_complete_outside_parameter_list() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing():\n    db\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 1,
                    character: 5,
                },
            )
            .unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn completes_same_file_fixture_decorated_with_imported_fixture() {
        let tmp = TempDir::new().unwrap();
        let test = tmp.path().join("tests/test_app.py");
        write(
            &test,
            "from pytest import fixture\n\n@fixture\ndef user(): pass\n\ndef test_thing(): pass\n",
        );
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 5,
                    character: 15,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "user"));
    }

    #[test]
    fn conftest_fixture_is_visible_to_nested_tests() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/sub/test_app.py");
        write(&test, "def test_thing(): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "db"));
    }

    #[test]
    fn sibling_conftest_fixture_is_not_visible() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/api/conftest.py"),
            "import pytest\n@pytest.fixture\ndef api_client(): pass\n",
        );
        let test = tmp.path().join("tests/web/test_app.py");
        write(&test, "def test_thing(): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap();
        assert!(!items.iter().any(|item| item.label == "api_client"));
    }

    #[test]
    fn completes_multiline_test_parameters() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing(\n    \n): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 1,
                    character: 4,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "db"));
    }

    #[test]
    fn completes_inside_multiline_params_with_defaults_and_annotations() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(
            &test,
            "def test_thing(\n    existing: str = call((1, 2)),\n    \n): pass\n",
        );
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 2,
                    character: 4,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "db"));
    }

    #[test]
    fn references_use_ast_parameter_ranges_not_string_matches() {
        let tmp = TempDir::new().unwrap();
        let conftest = tmp.path().join("tests/conftest.py");
        write(
            &conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing(db_name, db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let refs = index
            .references(
                &conftest,
                Position {
                    line: 2,
                    character: 5,
                },
            )
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].range.start.character, 24);
    }

    #[test]
    fn async_fixture_is_discovered() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\nasync def async_db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing(): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "async_db"));
    }

    #[test]
    fn nearest_conftest_fixture_wins_for_definition() {
        let tmp = TempDir::new().unwrap();
        let root_conftest = tmp.path().join("tests/conftest.py");
        let nested_conftest = tmp.path().join("tests/sub/conftest.py");
        write(
            &root_conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        write(
            &nested_conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/sub/test_app.py");
        write(&test, "def test_thing(db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(nested_conftest));
    }

    #[test]
    fn star_imported_conftest_reexport_fixture_is_visible() {
        let tmp = TempDir::new().unwrap();
        let fixture_module = tmp.path().join("project/testing/fixtures/clients.py");
        write(
            &fixture_module,
            "import pytest\n@pytest.fixture\ndef api_client(): pass\n",
        );
        write(
            &tmp.path().join("project/conftest.py"),
            "from project.testing.fixtures.clients import api_client\n",
        );
        write(
            &tmp.path()
                .join("project/features/widgets/tests/conftest.py"),
            "from project.conftest import *\n",
        );
        let test = tmp
            .path()
            .join("project/features/widgets/tests/test_widgets.py");
        write(&test, "def test_create_widget(api_client): pass\n");

        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 0,
                    character: 24,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(fixture_module));
    }

    #[test]
    fn imported_fixture_is_visible() {
        let tmp = TempDir::new().unwrap();
        let helper = tmp.path().join("tests/fixtures.py");
        write(
            &helper,
            "import pytest\n@pytest.fixture\ndef user(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(
            &test,
            "from tests.fixtures import user\n\ndef test_thing(user): pass\n",
        );
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 2,
                    character: 15,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(helper));
    }

    #[test]
    fn pytest_plugins_fixture_is_visible() {
        let tmp = TempDir::new().unwrap();
        let plugin = tmp.path().join("tests/plugins/db.py");
        write(
            &plugin,
            "import pytest\n@pytest.fixture\ndef plugin_db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(
            &test,
            "pytest_plugins = [\n    'tests.plugins.db',\n]\n\ndef test_thing(plugin_db): pass\n",
        );
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 4,
                    character: 17,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(plugin));
    }

    #[test]
    fn ancestor_conftest_pytest_plugins_fixture_is_visible() {
        let tmp = TempDir::new().unwrap();
        let plugin = tmp.path().join("tests/cohorts/integration.py");
        write(
            &plugin,
            "import pytest\n@pytest.fixture\ndef integration_env(): pass\n",
        );
        write(
            &tmp.path().join("app/modules/conftest.py"),
            "pytest_plugins = [\n    'tests.cohorts.integration',\n]\n",
        );
        let test = tmp
            .path()
            .join("app/modules/evaluations/tests/test_eval.py");
        write(
            &test,
            "def test_thing(\n    integration_env: Settings,\n): pass\n",
        );

        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 1,
                    character: 5,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(plugin));
    }

    #[test]
    fn references_from_fixture_definition_find_visible_test_params() {
        let tmp = TempDir::new().unwrap();
        let conftest = tmp.path().join("tests/conftest.py");
        write(
            &conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test_one = tmp.path().join("tests/test_one.py");
        let test_two = tmp.path().join("tests/sub/test_two.py");
        let sibling = tmp.path().join("other/test_other.py");
        write(&test_one, "def test_one(db): pass\n");
        write(&test_two, "def test_two(\n    db,\n): pass\n");
        write(&sibling, "def test_other(db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let refs = index
            .references(
                &conftest,
                Position {
                    line: 2,
                    character: 5,
                },
            )
            .unwrap();
        let uris: Vec<_> = refs.iter().map(|loc| loc.uri.clone()).collect();
        assert!(uris.contains(&file_url(test_one)));
        assert!(uris.contains(&file_url(test_two)));
        assert!(!uris.contains(&file_url(sibling)));
    }

    #[test]
    fn references_from_fixture_parameter_find_other_usages() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test_one = tmp.path().join("tests/test_one.py");
        let test_two = tmp.path().join("tests/test_two.py");
        write(&test_one, "def test_one(db): pass\n");
        write(&test_two, "def test_two(db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let refs = index
            .references(
                &test_one,
                Position {
                    line: 0,
                    character: 13,
                },
            )
            .unwrap();
        assert_eq!(refs.len(), 2);
        let test_one_uri = file_url(test_one);
        let test_two_uri = file_url(test_two);
        assert!(refs.iter().any(|loc| loc.uri == test_one_uri));
        assert!(refs.iter().any(|loc| loc.uri == test_two_uri));
    }

    #[test]
    fn references_include_fixture_dependencies() {
        let tmp = TempDir::new().unwrap();
        let conftest = tmp.path().join("tests/conftest.py");
        write(
            &conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n\n@pytest.fixture\ndef user(db): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_user(db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let refs = index
            .references(
                &conftest,
                Position {
                    line: 2,
                    character: 5,
                },
            )
            .unwrap();
        assert_eq!(refs.len(), 2);
        let conftest_uri = file_url(conftest);
        let test_uri = file_url(test);
        assert!(refs
            .iter()
            .any(|loc| loc.uri == conftest_uri && loc.range.start.line == 5));
        assert!(refs.iter().any(|loc| loc.uri == test_uri));
    }

    #[test]
    fn definition_resolves_fixture_parameter_to_conftest() {
        let tmp = TempDir::new().unwrap();
        let conftest = tmp.path().join("tests/conftest.py");
        write(
            &conftest,
            "import pytest\n@pytest.fixture\ndef db(): pass\n",
        );
        let test = tmp.path().join("tests/test_app.py");
        write(&test, "def test_thing(db): pass\n");
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let location = index
            .definition(
                &test,
                Position {
                    line: 0,
                    character: 15,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.uri, file_url(conftest));
    }

    #[test]
    fn fixtures_capture_return_annotation_and_imports() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("tests/conftest.py");
        write(
            &file,
            "import pytest
from starlette.testclient import TestClient

@pytest.fixture
def admin_client() -> TestClient: pass
",
        );
        let fixtures = fixtures_in_text(&file, &fs::read_to_string(&file).unwrap());
        let ann = fixtures[0].return_annotation.as_ref().unwrap();
        assert_eq!(ann.text, "TestClient");
        assert_eq!(
            ann.imports,
            vec![ImportRequirement {
                module: "starlette.testclient".into(),
                name: "TestClient".into(),
                alias: None,
            }]
        );
    }

    #[test]
    fn diagnostics_report_missing_and_mismatched_fixture_annotations() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
@pytest.fixture
def db() -> Session: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_one(db): pass
def test_two(db: str): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let diags = index.annotation_diagnostics_for_text(&test, text).unwrap();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].kind, AnnotationDiagnosticKind::Missing);
        assert_eq!(diags[1].kind, AnnotationDiagnosticKind::Mismatched);
    }

    #[test]
    fn fixture_annotation_diagnostics_unwrap_generator_yield_type() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from collections.abc import AsyncGenerator
from dishka import AsyncContainer

@pytest.fixture
async def create_di_container() -> AsyncGenerator[AsyncContainer]:
    yield AsyncContainer()
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "from dishka import AsyncContainer

def test_one(create_di_container: AsyncContainer): pass
def test_two(create_di_container): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let diags = index.annotation_diagnostics_for_text(&test, text).unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].kind, AnnotationDiagnosticKind::Missing);
        assert_eq!(diags[0].fixture_annotation, "AsyncContainer");
    }

    #[test]
    fn fixture_annotation_code_action_imports_unwrapped_generator_yield_type() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from collections.abc import AsyncGenerator
from dishka import AsyncContainer

@pytest.fixture
async def create_di_container() -> AsyncGenerator[AsyncContainer]:
    yield AsyncContainer()
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_one(create_di_container): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let action = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits.iter().any(|edit| edit.new_text == ": AsyncContainer"));
        assert!(edits
            .iter()
            .any(|edit| edit.new_text.contains("from dishka import AsyncContainer")));
        assert!(!edits
            .iter()
            .any(|edit| edit.new_text.contains("AsyncGenerator")));
    }

    #[test]
    fn code_actions_filter_to_requested_parameter_range() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
@pytest.fixture
def admin_client() -> TestClient: pass
@pytest.fixture
def root_client() -> RootClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_user(admin_client, root_client): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(
                &test,
                text,
                file_url(&test),
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 14,
                    },
                    end: Position {
                        line: 0,
                        character: 14,
                    },
                }),
            )
            .unwrap();
        let quickfixes: Vec<_> = actions
            .iter()
            .filter(|action| action.title == "Add fixture type annotation")
            .collect();
        assert_eq!(quickfixes.len(), 1);
        let edits = quickfixes[0]
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits.iter().any(|edit| edit.new_text == ": TestClient"));
        assert!(!edits.iter().any(|edit| edit.new_text == ": RootClient"));
    }

    #[test]
    fn whole_file_action_coalesces_multiple_imports_at_same_position() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from app.clients import AdminClient
from app.clients import RootClient
@pytest.fixture
def admin_client() -> AdminClient: pass
@pytest.fixture
def root_client() -> RootClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_user(admin_client, root_client): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let file_action = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotations in file")
            .unwrap();
        let edits = file_action
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        let import_edits: Vec<_> = edits
            .iter()
            .filter(|edit| {
                edit.range.start.line == 0
                    && edit.range.start.character == 0
                    && edit.range.start == edit.range.end
            })
            .collect();
        assert_eq!(import_edits.len(), 1);
        assert!(import_edits[0]
            .new_text
            .contains("from app.clients import AdminClient\n"));
        assert!(import_edits[0]
            .new_text
            .contains("from app.clients import RootClient\n"));
    }

    #[test]
    fn complex_fixture_annotation_imports_all_referenced_symbols() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from collections.abc import Callable
from typing import Optional
from app.models import SomeType
from app.results import AnotherType

@pytest.fixture
def factory() -> Callable[[Optional[SomeType]], AnotherType]: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_factory(factory): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(
                &test,
                text,
                file_url(&test),
                Some(Range {
                    start: Position {
                        line: 0,
                        character: 17,
                    },
                    end: Position {
                        line: 0,
                        character: 17,
                    },
                }),
            )
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        let edit_text = edits
            .iter()
            .map(|edit| edit.new_text.as_str())
            .collect::<String>();
        assert!(edit_text.contains("from app.models import SomeType\n"));
        assert!(edit_text.contains("from app.results import AnotherType\n"));
        assert!(edit_text.contains("from collections.abc import Callable\n"));
        assert!(edit_text.contains("from typing import Optional\n"));
        assert!(edits
            .iter()
            .any(|edit| edit.new_text == ": Callable[[Optional[SomeType]], AnotherType]"));
    }

    #[test]
    fn local_fixture_return_type_is_imported_from_fixture_module() {
        let tmp = TempDir::new().unwrap();
        write(&tmp.path().join("tests/__init__.py"), "");
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest

class LocalClient: pass

@pytest.fixture
def local_client() -> LocalClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_local(local_client): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits
            .iter()
            .any(|edit| edit.new_text == "from tests.conftest import LocalClient\n"));
        assert!(edits.iter().any(|edit| edit.new_text == ": LocalClient"));
    }

    #[test]
    fn imports_are_inserted_after_multiline_import_block() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from fastapi.testclient import TestClient

@pytest.fixture
def client() -> TestClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "from app.module import (\n    Existing,\n)\n\ndef test_client(client): pass\n";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        let import_edit = edits
            .iter()
            .find(|edit| edit.new_text == "from fastapi.testclient import TestClient\n")
            .unwrap();
        assert_eq!(import_edit.range.start.line, 3);
        assert_eq!(import_edit.range.start.character, 0);
    }

    #[test]
    fn aliased_fixture_annotation_preserves_existing_alias_import() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from app.models import User as ModelUser

@pytest.fixture
def user() -> ModelUser: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_user(user): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits
            .iter()
            .any(|edit| edit.new_text == "from app.models import User as ModelUser\n"));
        assert!(edits.iter().any(|edit| edit.new_text == ": ModelUser"));
    }

    #[test]
    fn imports_are_inserted_after_module_docstring_and_future_imports() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from fastapi.testclient import TestClient

@pytest.fixture
def client() -> TestClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "\"\"\"module docs\"\"\"\nfrom __future__ import annotations\n\ndef test_client(client): pass\n";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        let import_edit = edits
            .iter()
            .find(|edit| edit.new_text == "from fastapi.testclient import TestClient\n")
            .unwrap();
        assert_eq!(import_edit.range.start.line, 2);
        assert_eq!(import_edit.range.start.character, 0);
    }

    #[test]
    fn imports_do_not_merge_into_existing_single_line_from_import() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from app.models import User

@pytest.fixture
def user() -> User: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "from app.models import Existing\n\ndef test_user(user): pass\n";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits
            .iter()
            .any(|edit| edit.new_text == "from app.models import User\n"));
        assert!(!edits.iter().any(|edit| edit.new_text == ", User"));
    }

    #[test]
    fn imports_do_not_merge_into_existing_multiline_from_import() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from app.models import User

@pytest.fixture
def user() -> User: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "from app.models import (\n    Existing,\n)\n\ndef test_user(user): pass\n";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text_range(&test, text, file_url(&test), None)
            .unwrap();
        let quickfix = actions
            .iter()
            .find(|action| action.title == "Add fixture type annotation")
            .unwrap();
        let edits = quickfix
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        let import_edit = edits
            .iter()
            .find(|edit| edit.new_text == "from app.models import User\n")
            .unwrap();
        assert_eq!(import_edit.range.start.line, 3);
        assert_eq!(import_edit.range.start.character, 0);
        assert!(!edits.iter().any(|edit| edit.new_text == "    User,\n"));
    }

    #[test]
    fn completion_does_not_insert_annotation_inside_existing_annotation_context() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
@pytest.fixture
def user() -> User: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_user(user: Us): pass\n";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions_for_text(
                &test,
                text,
                Position {
                    line: 0,
                    character: 22,
                },
            )
            .unwrap();
        let item = items.iter().find(|item| item.label == "user").unwrap();
        assert_eq!(item.insert_text.as_deref(), Some("user"));
        assert!(item.additional_text_edits.is_none());
    }

    #[test]
    fn code_actions_add_import_and_replace_or_insert_annotation() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from app.models import User
@pytest.fixture
def user() -> User: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_user(user): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let actions = index
            .code_actions_for_text(&test, text, file_url(&test))
            .unwrap();
        let edits = actions[0]
            .edit
            .as_ref()
            .unwrap()
            .changes
            .as_ref()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert!(edits.iter().any(|e| e.new_text
            == "from app.models import User
"));
        assert!(edits.iter().any(|e| e.new_text == ": User"));
    }

    #[test]
    fn imported_and_qualified_annotations_are_equivalent() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from starlette.testclient import TestClient
@pytest.fixture
def admin_client() -> TestClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "import starlette.testclient

def test_x(admin_client: starlette.testclient.TestClient): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let diags = index.annotation_diagnostics_for_text(&test, text).unwrap();
        assert!(diags.is_empty());
    }

    #[test]
    fn completion_inserts_annotation_and_import_edit() {
        let tmp = TempDir::new().unwrap();
        write(
            &tmp.path().join("tests/conftest.py"),
            "import pytest
from starlette.testclient import TestClient
@pytest.fixture
def admin_client() -> TestClient: pass
",
        );
        let test = tmp.path().join("tests/test_app.py");
        let text = "def test_x(admin_): pass
";
        write(&test, text);
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions_for_text(
                &test,
                text,
                Position {
                    line: 0,
                    character: 17,
                },
            )
            .unwrap();
        let item = items.iter().find(|i| i.label == "admin_client").unwrap();
        assert_eq!(
            item.insert_text.as_deref(),
            Some("admin_client: TestClient")
        );
        assert!(item
            .additional_text_edits
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.new_text
                == "from starlette.testclient import TestClient
"));
    }
}

#[derive(Debug, Clone)]
struct ParameterAnnotationInfo {
    name: String,
    name_range: Range,
    annotation: Option<(String, Range)>,
}

fn fixture_return_annotation(
    path: &Path,
    text: &str,
    func: &StmtFunctionDef,
) -> Option<FixtureAnnotation> {
    let returns = func.returns.as_ref()?;
    let annotation_text = source_for_range(text, returns.range())?.trim().to_string();
    Some(FixtureAnnotation {
        imports: imports_for_annotation(path, text, &annotation_text),
        text: annotation_text,
    })
}

fn source_for_range(text: &str, range: ruff_text_size::TextRange) -> Option<&str> {
    let start = u32::from(range.start()) as usize;
    let end = u32::from(range.end()) as usize;
    text.get(start..end)
}

fn parameter_annotation_infos(text: &str) -> Vec<ParameterAnnotationInfo> {
    let Ok(parsed) = parse_module(text) else {
        return Vec::new();
    };
    if parsed.has_invalid_syntax() {
        return Vec::new();
    }
    let mut infos = Vec::new();
    collect_parameter_annotation_infos(&parsed.syntax().body, text, &mut infos);
    infos
}

fn collect_parameter_annotation_infos(
    body: &[Stmt],
    text: &str,
    infos: &mut Vec<ParameterAnnotationInfo>,
) {
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                for parameter in func.parameters.iter() {
                    if let Some(info) = parameter_annotation_info(text, parameter.as_parameter()) {
                        infos.push(info);
                    }
                }
                collect_parameter_annotation_infos(&func.body, text, infos);
            }
            Stmt::ClassDef(class) => collect_parameter_annotation_infos(&class.body, text, infos),
            _ => {}
        }
    }
}

fn parameter_annotation_info(text: &str, parameter: &Parameter) -> Option<ParameterAnnotationInfo> {
    let name = parameter.name().as_str().to_string();
    let param_src = source_for_range(text, parameter.range())?;
    let param_start = u32::from(parameter.range().start()) as usize;
    let name_offset = param_src.find(&name)?;
    let name_range = Range {
        start: byte_to_position(text, TextSize::new((param_start + name_offset) as u32)),
        end: byte_to_position(
            text,
            TextSize::new((param_start + name_offset + name.len()) as u32),
        ),
    };
    let annotation = parameter.annotation().and_then(|ann| {
        let ann_text = source_for_range(text, ann.range())?.trim().to_string();
        Some((ann_text, range_for_text_range(text, ann.range())))
    });
    Some(ParameterAnnotationInfo {
        name,
        name_range,
        annotation,
    })
}

fn imports_for_annotation(path: &Path, text: &str, annotation: &str) -> Vec<ImportRequirement> {
    let imported = import_resolution_map(path, text);
    let mut result = Vec::new();
    for symbol in annotation_symbols(annotation) {
        if let Some(req) = imported.get(&symbol) {
            result.push(req.clone());
        } else if is_likely_local_type(&symbol, text) {
            if let Some(module) = module_name_for_path(path) {
                result.push(ImportRequirement {
                    module,
                    name: symbol,
                    alias: None,
                });
            }
        }
    }
    result.sort_by(|a, b| {
        a.module
            .cmp(&b.module)
            .then(a.name.cmp(&b.name))
            .then(a.alias.cmp(&b.alias))
    });
    result.dedup();
    result
}

fn import_resolution_map(path: &Path, text: &str) -> HashMap<String, ImportRequirement> {
    let Ok(parsed) = parse_module(text) else {
        return HashMap::new();
    };
    if parsed.has_invalid_syntax() {
        return HashMap::new();
    }
    let mut map = HashMap::new();
    for stmt in &parsed.syntax().body {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let module = alias.name.as_str().to_string();
                    let visible = alias
                        .asname
                        .as_ref()
                        .map(|a| a.as_str())
                        .unwrap_or_else(|| module.split('.').next().unwrap_or(&module));
                    map.insert(
                        visible.to_string(),
                        ImportRequirement {
                            module,
                            name: String::new(),
                            alias: alias.asname.as_ref().map(|a| a.as_str().to_string()),
                        },
                    );
                }
            }
            Stmt::ImportFrom(import_from) => {
                let module = resolve_import_from_module(
                    path,
                    import_from.level,
                    import_from.module.as_ref().map(|m| m.as_str()),
                );
                let Some(module) = module else {
                    continue;
                };
                for alias in &import_from.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    let visible = alias
                        .asname
                        .as_ref()
                        .unwrap_or(&alias.name)
                        .as_str()
                        .to_string();
                    map.insert(
                        visible,
                        ImportRequirement {
                            module: module.clone(),
                            name: alias.name.as_str().to_string(),
                            alias: alias.asname.as_ref().map(|a| a.as_str().to_string()),
                        },
                    );
                }
            }
            _ => {}
        }
    }
    map
}

fn resolve_import_from_module(path: &Path, level: u32, module: Option<&str>) -> Option<String> {
    if level == 0 {
        return module.map(str::to_string);
    }
    let current = module_name_for_path(path)?;
    let mut parts: Vec<&str> = current.split('.').collect();
    parts.pop();
    for _ in 1..level {
        parts.pop();
    }
    if let Some(module) = module {
        if !module.is_empty() {
            parts.extend(module.split('.'));
        }
    }
    (!parts.is_empty()).then(|| parts.join("."))
}

fn module_name_for_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts = if stem == "__init__" {
        Vec::new()
    } else {
        vec![stem.to_string()]
    };
    let mut dir = path.parent()?;
    while dir.join("__init__.py").exists() {
        parts.push(dir.file_name()?.to_str()?.to_string());
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }
    parts.reverse();
    if parts.is_empty() {
        path.parent()?.file_name()?.to_str().map(str::to_string)
    } else {
        Some(parts.join("."))
    }
}

fn is_likely_local_type(symbol: &str, text: &str) -> bool {
    text.contains(&format!("class {symbol}")) || text.contains(&format!("{symbol} ="))
}

fn annotation_symbols(annotation: &str) -> Vec<String> {
    let builtins = [
        "None", "list", "dict", "set", "tuple", "str", "int", "float", "bool", "bytes",
    ];
    let mut symbols = Vec::new();
    let bytes = annotation.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' || bytes[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len() && is_ident(bytes[i]) {
                i += 1;
            }
            let Some(name) = annotation.get(start..i) else {
                continue;
            };
            let prev = annotation
                .get(..start)
                .unwrap_or_default()
                .chars()
                .rev()
                .find(|c| !c.is_whitespace());
            if prev != Some('.') && !builtins.contains(&name) && !symbols.iter().any(|s| s == name)
            {
                symbols.push(name.to_string());
            }
        } else {
            i += 1;
        }
    }
    symbols
}

fn import_text_edits(text: &str, annotation: &FixtureAnnotation) -> Vec<TextEdit> {
    let mut edits = Vec::new();
    for req in &annotation.imports {
        if import_exists(text, req) {
            continue;
        }
        let line = import_insert_line(text);
        let new_text = if req.name.is_empty() {
            match &req.alias {
                Some(alias) => format!("import {} as {}\n", req.module, alias),
                None => format!("import {}\n", req.module),
            }
        } else {
            match &req.alias {
                Some(alias) => format!("from {} import {} as {}\n", req.module, req.name, alias),
                None => format!("from {} import {}\n", req.module, req.name),
            }
        };
        edits.push(TextEdit {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 0 },
            },
            new_text,
        });
    }
    edits
}

fn import_exists(text: &str, req: &ImportRequirement) -> bool {
    if req.name.is_empty() {
        match &req.alias {
            Some(alias) => text
                .lines()
                .any(|l| l.trim() == format!("import {} as {}", req.module, alias)),
            None => text
                .lines()
                .any(|l| l.trim() == format!("import {}", req.module)),
        }
    } else {
        import_name_exists(text, &req.module, &req.name, req.alias.as_deref())
    }
}

fn import_name_exists(text: &str, module: &str, name: &str, alias: Option<&str>) -> bool {
    let Ok(parsed) = parse_module(text) else {
        return false;
    };
    if parsed.has_invalid_syntax() {
        return false;
    }
    parsed.syntax().body.iter().any(|stmt| {
        let Stmt::ImportFrom(import_from) = stmt else {
            return false;
        };
        import_from.level == 0
            && import_from
                .module
                .as_ref()
                .is_some_and(|m| m.as_str() == module)
            && import_from.names.iter().any(|candidate| {
                candidate.name.as_str() == name
                    && candidate.asname.as_ref().map(|a| a.as_str()) == alias
            })
    })
}

fn import_insert_line(text: &str) -> u32 {
    if let Ok(parsed) = parse_module(text) {
        if !parsed.has_invalid_syntax() {
            let mut line = 0;
            for (idx, stmt) in parsed.syntax().body.iter().enumerate() {
                let source = source_for_range(text, stmt.range()).unwrap_or_default();
                if idx == 0 && looks_like_string_literal_stmt(source) {
                    let end = range_for_text_range(text, stmt.range()).end;
                    line = if end.character == 0 {
                        end.line
                    } else {
                        end.line + 1
                    };
                } else if matches!(stmt, Stmt::Import(_) | Stmt::ImportFrom(_)) {
                    let end = range_for_text_range(text, stmt.range()).end;
                    line = if end.character == 0 {
                        end.line
                    } else {
                        end.line + 1
                    };
                } else if !source.trim().is_empty() {
                    break;
                }
            }
            return line;
        }
    }

    let mut last = 0;
    for (idx, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("import ") || t.starts_with("from ") {
            last = idx as u32 + 1;
            if t.ends_with('(') {
                for (nested_idx, nested_line) in text.lines().enumerate().skip(idx + 1) {
                    last = nested_idx as u32 + 1;
                    if nested_line.trim_end().ends_with(')') {
                        break;
                    }
                }
            }
        } else if t.is_empty() && idx == last as usize {
            last = idx as u32 + 1;
        } else if !t.is_empty() {
            break;
        }
    }
    last
}

fn looks_like_string_literal_stmt(source: &str) -> bool {
    let trimmed = source.trim_start();
    trimmed.starts_with('"')
        || trimmed.starts_with('\'')
        || trimmed.starts_with("r\"")
        || trimmed.starts_with("r'")
        || trimmed.starts_with("R\"")
        || trimmed.starts_with("R'")
}

fn edits_for_annotation(
    text: &str,
    diag: &FixtureAnnotationDiagnostic,
    fixture: &Fixture,
) -> Vec<TextEdit> {
    let Some(annotation) = fixture_value_annotation(fixture) else {
        return Vec::new();
    };
    let mut edits = import_text_edits(text, &annotation);
    let new_text = match diag.kind {
        AnnotationDiagnosticKind::Missing => format!(": {}", diag.fixture_annotation),
        AnnotationDiagnosticKind::Mismatched => diag.fixture_annotation.clone(),
    };
    let range = match diag.kind {
        AnnotationDiagnosticKind::Missing => Range {
            start: diag.edit_range.end,
            end: diag.edit_range.end,
        },
        AnnotationDiagnosticKind::Mismatched => diag.edit_range,
    };
    edits.push(TextEdit { range, new_text });
    edits
}

fn coalesce_text_edits(edits: Vec<TextEdit>) -> Vec<TextEdit> {
    let mut seen_exact = HashSet::new();
    let mut result: Vec<TextEdit> = Vec::new();
    for edit in edits {
        let key = (
            edit.range.start.line,
            edit.range.start.character,
            edit.range.end.line,
            edit.range.end.character,
            edit.new_text.clone(),
        );
        if !seen_exact.insert(key) {
            continue;
        }
        if edit.range.start == edit.range.end {
            if let Some(existing) = result
                .iter_mut()
                .find(|existing| existing.range == edit.range)
            {
                existing.new_text.push_str(&edit.new_text);
                continue;
            }
        }
        result.push(edit);
    }
    result
}

fn ranges_intersect_or_touch_cursor(diagnostic: Range, requested: Range) -> bool {
    if requested.start == requested.end {
        return position_in_range(requested.start, diagnostic);
    }
    position_le(diagnostic.start, requested.end) && position_le(requested.start, diagnostic.end)
}

fn position_le(left: Position, right: Position) -> bool {
    left.line < right.line || left.line == right.line && left.character <= right.character
}

fn workspace_edit(uri: Url, edits: Vec<TextEdit>) -> WorkspaceEdit {
    let mut changes = HashMap::new();
    changes.insert(uri, edits);
    WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    }
}

fn fixture_value_annotation(fixture: &Fixture) -> Option<FixtureAnnotation> {
    let annotation = fixture.return_annotation.as_ref()?;
    let text = unwrap_fixture_value_annotation(&annotation.text)
        .unwrap_or_else(|| annotation.text.clone());
    let imports = annotation
        .imports
        .iter()
        .filter(|import| annotation_uses_import(&text, import))
        .cloned()
        .collect();
    Some(FixtureAnnotation { text, imports })
}

fn unwrap_fixture_value_annotation(annotation: &str) -> Option<String> {
    let normalized = annotation.split_whitespace().collect::<String>();
    let bracket_start = normalized.find('[')?;
    let bracket_end = normalized.rfind(']')?;
    if bracket_end <= bracket_start {
        return None;
    }
    let wrapper = normalized[..bracket_start].rsplit('.').next()?;
    if !matches!(
        wrapper,
        "Generator" | "Iterator" | "AsyncGenerator" | "AsyncIterator"
    ) {
        return None;
    }
    split_top_level_args(&normalized[bracket_start + 1..bracket_end])
        .into_iter()
        .next()
        .filter(|arg| !arg.is_empty())
}

fn split_top_level_args(args: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    for (idx, ch) in args.char_indices() {
        match ch {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                result.push(args[start..idx].trim().to_string());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    result.push(args[start..].trim().to_string());
    result
}

fn annotation_uses_import(annotation: &str, import: &ImportRequirement) -> bool {
    let visible = import
        .alias
        .as_deref()
        .or_else(|| {
            if import.name.is_empty() {
                import.module.split('.').next_back()
            } else {
                Some(import.name.as_str())
            }
        })
        .unwrap_or_default();
    contains_word(annotation, visible)
}

fn contains_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i] == b'_' || bytes[i].is_ascii_alphabetic())
            && text.get(i..).is_some_and(|suffix| suffix.starts_with(word))
        {
            let end = i + word.len();
            let before_ok = i == 0 || !is_ident(bytes[i - 1]);
            let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn annotations_equivalent(
    left: &str,
    right: &str,
    test_file: &Path,
    test_text: &str,
    fixture: &Fixture,
) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<String>();
    if norm(left) == norm(right) {
        return true;
    }
    let test_imports = import_resolution_map(test_file, test_text);
    let fixture_imports = fs::read_to_string(&fixture.path)
        .map(|s| import_resolution_map(&fixture.path, &s))
        .unwrap_or_default();
    canonical_annotation(left, &test_imports) == canonical_annotation(right, &fixture_imports)
}

fn canonical_annotation(annotation: &str, imports: &HashMap<String, ImportRequirement>) -> String {
    let mut out = annotation.split_whitespace().collect::<String>();
    for (visible, req) in imports {
        if req.name.is_empty()
            && req
                .module
                .split('.')
                .next()
                .is_some_and(|first| first == visible)
        {
            continue;
        }
        let replacement = if req.name.is_empty() {
            req.module.clone()
        } else {
            format!("{}.{}", req.module, req.name)
        };
        out = replace_word(&out, visible, &replacement);
    }
    out
}

fn replace_word(text: &str, word: &str, replacement: &str) -> String {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i] == b'_' || bytes[i].is_ascii_alphabetic())
            && text.get(i..).is_some_and(|suffix| suffix.starts_with(word))
        {
            let end = i + word.len();
            let before_ok = i == 0 || !is_ident(bytes[i - 1]);
            let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
            if before_ok && after_ok {
                out.push_str(replacement);
                i = end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
