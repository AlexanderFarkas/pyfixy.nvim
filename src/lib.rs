use anyhow::Result;
use lsp_types::{CompletionItem, CompletionItemKind, Location, Position, Range, Url};
use ruff_python_ast::Decorator;
use ruff_python_ast::{Expr, ExprCall, ExprName, ExprStringLiteral, Stmt, StmtFunctionDef};
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
}

#[derive(Debug, Default)]
pub struct FixtureIndex {
    root: PathBuf,
    fixtures: Vec<Fixture>,
}

impl FixtureIndex {
    pub fn build(root: &Path) -> Result<Self> {
        let mut fixtures = Vec::new();
        collect_fixtures(root, root, &mut fixtures)?;
        expand_conftest_reexports(root, &mut fixtures)?;
        Ok(Self {
            root: root.to_path_buf(),
            fixtures,
        })
    }

    pub fn completions(&self, file: &Path, position: Position) -> Result<Vec<CompletionItem>> {
        let text = fs::read_to_string(file)?;
        self.completions_for_text(file, &text, position)
    }

    pub fn completions_for_text(
        &self,
        file: &Path,
        text: &str,
        position: Position,
    ) -> Result<Vec<CompletionItem>> {
        if !is_in_function_params(text, position) {
            return Ok(Vec::new());
        }
        Ok(self
            .visible_fixtures_for_text(file, text)
            .into_iter()
            .map(|f| CompletionItem {
                label: f.name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(format!("pytest fixture ({})", f.path.display())),
                ..Default::default()
            })
            .collect())
    }

    pub fn definition(&self, file: &Path, position: Position) -> Result<Option<Location>> {
        let text = fs::read_to_string(file)?;
        let Some(name) = identifier_at_position(&text, position) else {
            return Ok(None);
        };
        if !is_in_function_params(&text, position) {
            return Ok(None);
        }
        let Some(fixture) = self
            .visible_fixtures(file)
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

    pub fn references(&self, file: &Path, position: Position) -> Result<Vec<Location>> {
        let Some(name) = self.fixture_name_at(file, position)? else {
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
        let text = fs::read_to_string(file)?;
        if is_in_function_params(&text, position) {
            let Some(name) = identifier_at_position(&text, position) else {
                return Ok(None);
            };
            return Ok(self
                .visible_fixtures(file)
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
        let text = fs::read_to_string(file).unwrap_or_default();
        self.visible_fixtures_for_text(file, &text)
    }

    fn visible_fixtures_for_text(&self, file: &Path, text: &str) -> Vec<&Fixture> {
        let file_dir = file.parent().unwrap_or_else(|| Path::new(""));
        let imports =
            imported_fixture_sources_from_text(file, text, &self.root).unwrap_or_default();
        let plugin_modules = pytest_plugin_modules(file).unwrap_or_default();
        let plugin_paths: HashSet<PathBuf> = plugin_modules
            .iter()
            .filter_map(|module| module_to_path(&self.root, module))
            .collect();

        let mut by_name: HashMap<&str, (&Fixture, usize)> = HashMap::new();
        for fixture in &self.fixtures {
            let Some(score) = fixture_visibility_score(
                fixture,
                file,
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
    let Ok(parsed) = parse_module(&text) else {
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
    let text = fs::read_to_string(file)?;
    let mut modules = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("pytest_plugins") {
            continue;
        }
        for quoted in extract_quoted_strings(trimmed) {
            modules.push(quoted);
        }
    }
    Ok(modules)
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch != '\'' && ch != '"' {
            continue;
        }
        let quote = ch;
        for (end, next) in chars.by_ref() {
            if next == quote {
                strings.push(text[start + 1..end].to_string());
                break;
            }
        }
    }
    strings
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
    let Ok(parsed) = parse_module(&text) else {
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
        assert_eq!(location.uri, Url::from_file_path(nested_conftest).unwrap());
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
        assert_eq!(location.uri, Url::from_file_path(fixture_module).unwrap());
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
        assert_eq!(location.uri, Url::from_file_path(helper).unwrap());
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
            "pytest_plugins = ['tests.plugins.db']\n\ndef test_thing(): pass\n",
        );
        let index = FixtureIndex::build(tmp.path()).unwrap();
        let items = index
            .completions(
                &test,
                Position {
                    line: 2,
                    character: 15,
                },
            )
            .unwrap();
        assert!(items.iter().any(|item| item.label == "plugin_db"));
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
        assert!(uris.contains(&Url::from_file_path(test_one).unwrap()));
        assert!(uris.contains(&Url::from_file_path(test_two).unwrap()));
        assert!(!uris.contains(&Url::from_file_path(sibling).unwrap()));
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
        let test_one_uri = Url::from_file_path(test_one).unwrap();
        let test_two_uri = Url::from_file_path(test_two).unwrap();
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
        let conftest_uri = Url::from_file_path(conftest).unwrap();
        let test_uri = Url::from_file_path(test).unwrap();
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
        assert_eq!(location.uri, Url::from_file_path(conftest).unwrap());
    }
}
