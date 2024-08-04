use std::borrow::Cow;
use std::iter::FusedIterator;

use ruff_db::files::{File, FilePath, FileRootKind};
use ruff_db::program::{Program, SearchPathSettings};
use ruff_db::system::{DirectoryEntry, System, SystemPath, SystemPathBuf};
use ruff_db::vendored::VendoredPath;
use rustc_hash::FxHashSet;

use crate::db::Db;
use crate::module::{Module, ModuleKind};
use crate::module_name::ModuleName;
use crate::path::{ModulePath, SearchPath};
use crate::state::ResolverState;

/// Resolves a module name to a module.
pub fn resolve_module(db: &dyn Db, module_name: ModuleName) -> Option<Module> {
    let interned_name = ModuleNameIngredient::new(db, module_name);

    resolve_module_query(db, interned_name)
}

/// Salsa query that resolves an interned [`ModuleNameIngredient`] to a module.
///
/// This query should not be called directly. Instead, use [`resolve_module`]. It only exists
/// because Salsa requires the module name to be an ingredient.
#[salsa::tracked]
pub(crate) fn resolve_module_query<'db>(
    db: &'db dyn Db,
    module_name: ModuleNameIngredient<'db>,
) -> Option<Module> {
    let name = module_name.name(db);
    let _span = tracing::trace_span!("resolve_module", %name).entered();

    let (search_path, module_file, kind) = resolve_name(db, name)?;

    let module = Module::new(name.clone(), kind, search_path, module_file);

    Some(module)
}

/// Resolves the module for the given path.
///
/// Returns `None` if the path is not a module locatable via any of the known search paths.
#[allow(unused)]
pub(crate) fn path_to_module(db: &dyn Db, path: &FilePath) -> Option<Module> {
    // It's not entirely clear on first sight why this method calls `file_to_module` instead of
    // it being the other way round, considering that the first thing that `file_to_module` does
    // is to retrieve the file's path.
    //
    // The reason is that `file_to_module` is a tracked Salsa query and salsa queries require that
    // all arguments are Salsa ingredients (something stored in Salsa). `Path`s aren't salsa ingredients but
    // `VfsFile` is. So what we do here is to retrieve the `path`'s `VfsFile` so that we can make
    // use of Salsa's caching and invalidation.
    let file = path.to_file(db.upcast())?;
    file_to_module(db, file)
}

#[derive(Debug, Clone, Copy)]
enum SystemOrVendoredPathRef<'a> {
    System(&'a SystemPath),
    Vendored(&'a VendoredPath),
}

/// Resolves the module for the file with the given id.
///
/// Returns `None` if the file is not a module locatable via any of the known search paths.
#[salsa::tracked]
pub(crate) fn file_to_module(db: &dyn Db, file: File) -> Option<Module> {
    let _span = tracing::trace_span!("file_to_module", ?file).entered();

    let path = match file.path(db.upcast()) {
        FilePath::System(system) => SystemOrVendoredPathRef::System(system),
        FilePath::Vendored(vendored) => SystemOrVendoredPathRef::Vendored(vendored),
        FilePath::SystemVirtual(_) => return None,
    };

    let mut search_paths = module_search_paths(db);

    let module_name = loop {
        let candidate = search_paths.next()?;
        let relative_path = match path {
            SystemOrVendoredPathRef::System(path) => candidate.relativize_system_path(path),
            SystemOrVendoredPathRef::Vendored(path) => candidate.relativize_vendored_path(path),
        };
        if let Some(relative_path) = relative_path {
            break relative_path.to_module_name()?;
        }
    };

    // Resolve the module name to see if Python would resolve the name to the same path.
    // If it doesn't, then that means that multiple modules have the same name in different
    // root paths, but that the module corresponding to `path` is in a lower priority search path,
    // in which case we ignore it.
    let module = resolve_module(db, module_name)?;

    if file == module.file() {
        Some(module)
    } else {
        // This path is for a module with the same name but with a different precedence. For example:
        // ```
        // src/foo.py
        // src/foo/__init__.py
        // ```
        // The module name of `src/foo.py` is `foo`, but the module loaded by Python is `src/foo/__init__.py`.
        // That means we need to ignore `src/foo.py` even though it resolves to the same module name.
        None
    }
}

/// Collect all dynamic search paths. For each `site-packages` path:
/// - Collect that `site-packages` path
/// - Collect any search paths listed in `.pth` files in that `site-packages` directory
///   due to editable installations of third-party packages.
///
/// The editable-install search paths for the first `site-packages` directory
/// should come between the two `site-packages` directories when it comes to
/// module-resolution priority.
#[salsa::tracked(return_ref)]
pub(crate) fn dynamic_resolution_paths(db: &dyn Db) -> Vec<SearchPath> {
    let SearchPathSettings {
        static_search_paths,
        site_packages_paths,
    } = Program::get(db).search_path_settings(db);

    let mut dynamic_paths = Vec::new();

    if site_packages_paths.is_empty() {
        return dynamic_paths;
    }

    let mut existing_paths: FxHashSet<_> = static_search_paths
        .iter()
        .filter_map(|path| path.as_system_path())
        .map(Cow::Borrowed)
        .collect();

    let files = db.files();
    let system = db.system();

    for site_packages_dir in site_packages_paths {
        if !existing_paths.insert(Cow::Borrowed(site_packages_dir)) {
            continue;
        }
        let site_packages_root = files.try_add_root(
            db.upcast(),
            site_packages_dir,
            FileRootKind::LibrarySearchPath,
        );
        // This query needs to be re-executed each time a `.pth` file
        // is added, modified or removed from the `site-packages` directory.
        // However, we don't use Salsa queries to read the source text of `.pth` files;
        // we use the APIs on the `System` trait directly. As such, add a dependency on the
        // site-package directory's revision.
        site_packages_root.revision(db.upcast());

        dynamic_paths
            .push(SearchPath::site_packages(system, site_packages_dir.to_owned()).unwrap());

        // As well as modules installed directly into `site-packages`,
        // the directory may also contain `.pth` files.
        // Each `.pth` file in `site-packages` may contain one or more lines
        // containing a (relative or absolute) path.
        // Each of these paths may point to an editable install of a package,
        // so should be considered an additional search path.
        let Ok(pth_file_iterator) = PthFileIterator::new(db, site_packages_dir) else {
            continue;
        };

        // The Python documentation specifies that `.pth` files in `site-packages`
        // are processed in alphabetical order, so collecting and then sorting is necessary.
        // https://docs.python.org/3/library/site.html#module-site
        let mut all_pth_files: Vec<PthFile> = pth_file_iterator.collect();
        all_pth_files.sort_by(|a, b| a.path.cmp(&b.path));

        for pth_file in &all_pth_files {
            for installation in pth_file.editable_installations() {
                if existing_paths.insert(Cow::Owned(
                    installation.as_system_path().unwrap().to_path_buf(),
                )) {
                    dynamic_paths.push(installation);
                }
            }
        }
    }

    dynamic_paths
}

/// Iterate over the available module-resolution search paths,
/// following the invariants maintained by [`sys.path` at runtime]:
/// "No item is added to `sys.path` more than once."
/// Dynamic search paths (required for editable installs into `site-packages`)
/// are only calculated lazily.
///
/// [`sys.path` at runtime]: https://docs.python.org/3/library/site.html#module-site
pub(crate) struct SearchPathIterator<'db> {
    db: &'db dyn Db,
    static_paths: std::slice::Iter<'db, ruff_db::program::SearchPath>,
    dynamic_paths: Option<std::slice::Iter<'db, SearchPath>>,
}

impl<'db> Iterator for SearchPathIterator<'db> {
    type Item = Cow<'db, SearchPath>;

    fn next(&mut self) -> Option<Self::Item> {
        let SearchPathIterator {
            db,
            static_paths,
            dynamic_paths,
        } = self;

        static_paths
            .next()
            .map(|path| Cow::Owned(SearchPath::from(path)))
            .or_else(|| {
                dynamic_paths
                    .get_or_insert_with(|| dynamic_resolution_paths(*db).iter())
                    .next()
                    .map(Cow::Borrowed)
            })
    }
}

impl<'db> FusedIterator for SearchPathIterator<'db> {}

/// Represents a single `.pth` file in a `site-packages` directory.
/// One or more lines in a `.pth` file may be a (relative or absolute)
/// path that represents an editable installation of a package.
struct PthFile<'db> {
    system: &'db dyn System,
    path: SystemPathBuf,
    contents: String,
    site_packages: &'db SystemPath,
}

impl<'db> PthFile<'db> {
    /// Yield paths in this `.pth` file that appear to represent editable installations,
    /// and should therefore be added as module-resolution search paths.
    fn editable_installations(&'db self) -> impl Iterator<Item = SearchPath> + 'db {
        let PthFile {
            system,
            path: _,
            contents,
            site_packages,
        } = self;

        // Empty lines or lines starting with '#' are ignored by the Python interpreter.
        // Lines that start with "import " or "import\t" do not represent editable installs at all;
        // instead, these are lines that are executed by Python at startup.
        // https://docs.python.org/3/library/site.html#module-site
        contents.lines().filter_map(move |line| {
            let line = line.trim_end();
            if line.is_empty()
                || line.starts_with('#')
                || line.starts_with("import ")
                || line.starts_with("import\t")
            {
                return None;
            }
            let possible_editable_install = SystemPath::absolute(line, site_packages);
            SearchPath::editable(*system, possible_editable_install).ok()
        })
    }
}

/// Iterator that yields a [`PthFile`] instance for every `.pth` file
/// found in a given `site-packages` directory.
struct PthFileIterator<'db> {
    db: &'db dyn Db,
    directory_iterator: Box<dyn Iterator<Item = std::io::Result<DirectoryEntry>> + 'db>,
    site_packages: &'db SystemPath,
}

impl<'db> PthFileIterator<'db> {
    fn new(db: &'db dyn Db, site_packages: &'db SystemPath) -> std::io::Result<Self> {
        Ok(Self {
            db,
            directory_iterator: db.system().read_directory(site_packages)?,
            site_packages,
        })
    }
}

impl<'db> Iterator for PthFileIterator<'db> {
    type Item = PthFile<'db>;

    fn next(&mut self) -> Option<Self::Item> {
        let PthFileIterator {
            db,
            directory_iterator,
            site_packages,
        } = self;

        let system = db.system();

        loop {
            let entry_result = directory_iterator.next()?;
            let Ok(entry) = entry_result else {
                continue;
            };
            let file_type = entry.file_type();
            if file_type.is_directory() {
                continue;
            }
            let path = entry.into_path();
            if path.extension() != Some("pth") {
                continue;
            }

            let Ok(contents) = db.system().read_to_string(&path) else {
                continue;
            };

            return Some(PthFile {
                system,
                path,
                contents,
                site_packages,
            });
        }
    }
}

pub(crate) fn module_search_paths(db: &dyn Db) -> SearchPathIterator {
    SearchPathIterator {
        db,
        static_paths: Program::get(db)
            .search_path_settings(db)
            .static_search_paths
            .iter(),
        dynamic_paths: None,
    }
}

/// A thin wrapper around `ModuleName` to make it a Salsa ingredient.
///
/// This is needed because Salsa requires that all query arguments are salsa ingredients.
#[salsa::interned]
struct ModuleNameIngredient<'db> {
    #[return_ref]
    pub(super) name: ModuleName,
}

/// Given a module name and a list of search paths in which to lookup modules,
/// attempt to resolve the module name
fn resolve_name(db: &dyn Db, name: &ModuleName) -> Option<(SearchPath, File, ModuleKind)> {
    let target_version = Program::get(db).target_version(db);
    let resolver_state = ResolverState::new(db, target_version);
    let (_, minor_version) = target_version.as_tuple();
    let is_builtin_module =
        ruff_python_stdlib::sys::is_builtin_module(minor_version, name.as_str());

    for search_path in module_search_paths(db) {
        // When a builtin module is imported, standard module resolution is bypassed:
        // the module name always resolves to the stdlib module,
        // even if there's a module of the same name in the first-party root
        // (which would normally result in the stdlib module being overridden).
        if is_builtin_module && !search_path.is_standard_library() {
            continue;
        }

        let mut components = name.components();
        let module_name = components.next_back()?;

        match resolve_package(&search_path, components, &resolver_state) {
            Ok(resolved_package) => {
                let mut package_path = resolved_package.path;

                package_path.push(module_name);

                // Must be a `__init__.pyi` or `__init__.py` or it isn't a package.
                let kind = if package_path.is_directory(&resolver_state) {
                    package_path.push("__init__");
                    ModuleKind::Package
                } else {
                    ModuleKind::Module
                };

                // TODO Implement full https://peps.python.org/pep-0561/#type-checker-module-resolution-order resolution
                if let Some(stub) = package_path.with_pyi_extension().to_file(&resolver_state) {
                    return Some((search_path.into_owned(), stub, kind));
                }

                if let Some(module) = package_path
                    .with_py_extension()
                    .and_then(|path| path.to_file(&resolver_state))
                {
                    return Some((search_path.into_owned(), module, kind));
                }

                // For regular packages, don't search the next search path. All files of that
                // package must be in the same location
                if resolved_package.kind.is_regular_package() {
                    return None;
                }
            }
            Err(parent_kind) => {
                if parent_kind.is_regular_package() {
                    // For regular packages, don't search the next search path.
                    return None;
                }
            }
        }
    }

    None
}

fn resolve_package<'a, 'db, I>(
    module_search_path: &SearchPath,
    components: I,
    resolver_state: &ResolverState<'db>,
) -> Result<ResolvedPackage, PackageKind>
where
    I: Iterator<Item = &'a str>,
{
    let mut package_path = module_search_path.to_module_path();

    // `true` if inside a folder that is a namespace package (has no `__init__.py`).
    // Namespace packages are special because they can be spread across multiple search paths.
    // https://peps.python.org/pep-0420/
    let mut in_namespace_package = false;

    // `true` if resolving a sub-package. For example, `true` when resolving `bar` of `foo.bar`.
    let mut in_sub_package = false;

    // For `foo.bar.baz`, test that `foo` and `baz` both contain a `__init__.py`.
    for folder in components {
        package_path.push(folder);

        let is_regular_package = package_path.is_regular_package(resolver_state);

        if is_regular_package {
            in_namespace_package = false;
        } else if package_path.is_directory(resolver_state) {
            // A directory without an `__init__.py` is a namespace package, continue with the next folder.
            in_namespace_package = true;
        } else if in_namespace_package {
            // Package not found but it is part of a namespace package.
            return Err(PackageKind::Namespace);
        } else if in_sub_package {
            // A regular sub package wasn't found.
            return Err(PackageKind::Regular);
        } else {
            // We couldn't find `foo` for `foo.bar.baz`, search the next search path.
            return Err(PackageKind::Root);
        }

        in_sub_package = true;
    }

    let kind = if in_namespace_package {
        PackageKind::Namespace
    } else if in_sub_package {
        PackageKind::Regular
    } else {
        PackageKind::Root
    };

    Ok(ResolvedPackage {
        kind,
        path: package_path,
    })
}

#[derive(Debug)]
struct ResolvedPackage {
    path: ModulePath,
    kind: PackageKind,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum PackageKind {
    /// A root package or module. E.g. `foo` in `foo.bar.baz` or just `foo`.
    Root,

    /// A regular sub-package where the parent contains an `__init__.py`.
    ///
    /// For example, `bar` in `foo.bar` when the `foo` directory contains an `__init__.py`.
    Regular,

    /// A sub-package in a namespace package. A namespace package is a package without an `__init__.py`.
    ///
    /// For example, `bar` in `foo.bar` if the `foo` directory contains no `__init__.py`.
    Namespace,
}

impl PackageKind {
    const fn is_regular_package(self) -> bool {
        matches!(self, PackageKind::Regular)
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::files::{system_path_to_file, File, FilePath};
    use ruff_db::program::{RawProgramSettings, RawSearchPathSettings, TargetVersion};
    use ruff_db::system::DbWithTestSystem;
    use ruff_db::testing::{
        assert_const_function_query_was_not_run, assert_function_query_was_not_run,
    };
    use ruff_db::Db;

    use crate::db::tests::TestDb;
    use crate::module::ModuleKind;
    use crate::module_name::ModuleName;
    use crate::program_from_raw_settings;
    use crate::testing::{FileSpec, MockedTypeshed, TestCase, TestCaseBuilder};

    use super::*;

    #[test]
    fn first_party_module() {
        let TestCase { db, src, .. } = TestCaseBuilder::new()
            .with_src_files(&[("foo.py", "print('Hello, world!')")])
            .build();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();

        assert_eq!(
            Some(&foo_module),
            resolve_module(&db, foo_module_name.clone()).as_ref()
        );

        assert_eq!("foo", foo_module.name());
        assert_eq!(&src, foo_module.search_path());
        assert_eq!(ModuleKind::Module, foo_module.kind());

        let expected_foo_path = src.join("foo.py");
        assert_eq!(&expected_foo_path, foo_module.file().path(&db));
        assert_eq!(
            Some(foo_module),
            path_to_module(&db, &FilePath::System(expected_foo_path))
        );
    }

    #[test]
    fn builtins_vendored() {
        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_vendored_typeshed()
            .with_src_files(&[("builtins.py", "FOOOO = 42")])
            .build();

        let builtins_module_name = ModuleName::new_static("builtins").unwrap();
        let builtins = resolve_module(&db, builtins_module_name).expect("builtins to resolve");

        assert_eq!(builtins.file().path(&db), &stdlib.join("builtins.pyi"));
    }

    #[test]
    fn builtins_custom() {
        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: &[("builtins.pyi", "def min(a, b): ...")],
            versions: "builtins: 3.8-",
        };

        const SRC: &[FileSpec] = &[("builtins.py", "FOOOO = 42")];

        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_src_files(SRC)
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let builtins_module_name = ModuleName::new_static("builtins").unwrap();
        let builtins = resolve_module(&db, builtins_module_name).expect("builtins to resolve");

        assert_eq!(builtins.file().path(&db), &stdlib.join("builtins.pyi"));
    }

    #[test]
    fn stdlib() {
        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: &[("functools.pyi", "def update_wrapper(): ...")],
            versions: "functools: 3.8-",
        };

        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let functools_module_name = ModuleName::new_static("functools").unwrap();
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();

        assert_eq!(
            Some(&functools_module),
            resolve_module(&db, functools_module_name).as_ref()
        );

        assert_eq!(&stdlib, functools_module.search_path());
        assert_eq!(ModuleKind::Module, functools_module.kind());

        let expected_functools_path = stdlib.join("functools.pyi");
        assert_eq!(&expected_functools_path, functools_module.file().path(&db));

        assert_eq!(
            Some(functools_module),
            path_to_module(&db, &FilePath::System(expected_functools_path))
        );
    }

    fn create_module_names(raw_names: &[&str]) -> Vec<ModuleName> {
        raw_names
            .iter()
            .map(|raw| ModuleName::new(raw).unwrap())
            .collect()
    }

    #[test]
    fn stdlib_resolution_respects_versions_file_py38_existing_modules() {
        const VERSIONS: &str = "\
            asyncio: 3.8-               # 'Regular' package on py38+
            asyncio.tasks: 3.9-3.11     # Submodule on py39+ only
            functools: 3.8-             # Top-level single-file module
            xml: 3.8-3.8                # Namespace package on py38 only
        ";

        const STDLIB: &[FileSpec] = &[
            ("asyncio/__init__.pyi", ""),
            ("asyncio/tasks.pyi", ""),
            ("functools.pyi", ""),
            ("xml/etree.pyi", ""),
        ];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: STDLIB,
            versions: VERSIONS,
        };

        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let existing_modules = create_module_names(&["asyncio", "functools", "xml.etree"]);
        for module_name in existing_modules {
            let resolved_module = resolve_module(&db, module_name.clone()).unwrap_or_else(|| {
                panic!("Expected module {module_name} to exist in the mock stdlib")
            });
            let search_path = resolved_module.search_path();
            assert_eq!(
                &stdlib, search_path,
                "Search path for {module_name} was unexpectedly {search_path:?}"
            );
            assert!(
                search_path.is_stdlib_search_path(),
                "Expected a stdlib search path, but got {search_path:?}"
            );
        }
    }

    #[test]
    fn stdlib_resolution_respects_versions_file_py38_nonexisting_modules() {
        const VERSIONS: &str = "\
            asyncio: 3.8-               # 'Regular' package on py38+
            asyncio.tasks: 3.9-3.11     # Submodule on py39+ only
            collections: 3.9-           # 'Regular' package on py39+
            importlib: 3.9-             # Namespace package on py39+
            xml: 3.8-3.8                # Namespace package on 3.8 only
        ";

        const STDLIB: &[FileSpec] = &[
            ("collections/__init__.pyi", ""),
            ("asyncio/__init__.pyi", ""),
            ("asyncio/tasks.pyi", ""),
            ("importlib/abc.pyi", ""),
            ("xml/etree.pyi", ""),
        ];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: STDLIB,
            versions: VERSIONS,
        };

        let TestCase { db, .. } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let nonexisting_modules = create_module_names(&[
            "collections",
            "importlib",
            "importlib.abc",
            "xml",
            "asyncio.tasks",
        ]);

        for module_name in nonexisting_modules {
            assert!(
                resolve_module(&db, module_name.clone()).is_none(),
                "Unexpectedly resolved a module for {module_name}"
            );
        }
    }

    #[test]
    fn stdlib_resolution_respects_versions_file_py39_existing_modules() {
        const VERSIONS: &str = "\
            asyncio: 3.8-               # 'Regular' package on py38+
            asyncio.tasks: 3.9-3.11     # Submodule on py39+ only
            collections: 3.9-           # 'Regular' package on py39+
            functools: 3.8-             # Top-level single-file module
            importlib: 3.9-             # Namespace package on py39+
        ";

        const STDLIB: &[FileSpec] = &[
            ("asyncio/__init__.pyi", ""),
            ("asyncio/tasks.pyi", ""),
            ("collections/__init__.pyi", ""),
            ("functools.pyi", ""),
            ("importlib/abc.pyi", ""),
        ];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: STDLIB,
            versions: VERSIONS,
        };

        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py39)
            .build();

        let existing_modules = create_module_names(&[
            "asyncio",
            "functools",
            "importlib.abc",
            "collections",
            "asyncio.tasks",
        ]);

        for module_name in existing_modules {
            let resolved_module = resolve_module(&db, module_name.clone()).unwrap_or_else(|| {
                panic!("Expected module {module_name} to exist in the mock stdlib")
            });
            let search_path = resolved_module.search_path();
            assert_eq!(
                &stdlib, search_path,
                "Search path for {module_name} was unexpectedly {search_path:?}"
            );
            assert!(
                search_path.is_stdlib_search_path(),
                "Expected a stdlib search path, but got {search_path:?}"
            );
        }
    }
    #[test]
    fn stdlib_resolution_respects_versions_file_py39_nonexisting_modules() {
        const VERSIONS: &str = "\
            importlib: 3.9-   # Namespace package on py39+
            xml: 3.8-3.8      # Namespace package on 3.8 only
        ";

        const STDLIB: &[FileSpec] = &[("importlib/abc.pyi", ""), ("xml/etree.pyi", "")];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: STDLIB,
            versions: VERSIONS,
        };

        let TestCase { db, .. } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py39)
            .build();

        let nonexisting_modules = create_module_names(&["importlib", "xml", "xml.etree"]);
        for module_name in nonexisting_modules {
            assert!(
                resolve_module(&db, module_name.clone()).is_none(),
                "Unexpectedly resolved a module for {module_name}"
            );
        }
    }

    #[test]
    fn first_party_precedence_over_stdlib() {
        const SRC: &[FileSpec] = &[("functools.py", "def update_wrapper(): ...")];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            stdlib_files: &[("functools.pyi", "def update_wrapper(): ...")],
            versions: "functools: 3.8-",
        };

        let TestCase { db, src, .. } = TestCaseBuilder::new()
            .with_src_files(SRC)
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let functools_module_name = ModuleName::new_static("functools").unwrap();
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();

        assert_eq!(
            Some(&functools_module),
            resolve_module(&db, functools_module_name).as_ref()
        );
        assert_eq!(&src, functools_module.search_path());
        assert_eq!(ModuleKind::Module, functools_module.kind());
        assert_eq!(&src.join("functools.py"), functools_module.file().path(&db));

        assert_eq!(
            Some(functools_module),
            path_to_module(&db, &FilePath::System(src.join("functools.py")))
        );
    }

    #[test]
    fn stdlib_uses_vendored_typeshed_when_no_custom_typeshed_supplied() {
        let TestCase { db, stdlib, .. } = TestCaseBuilder::new()
            .with_vendored_typeshed()
            .with_target_version(TargetVersion::default())
            .build();

        let pydoc_data_topics_name = ModuleName::new_static("pydoc_data.topics").unwrap();
        let pydoc_data_topics = resolve_module(&db, pydoc_data_topics_name).unwrap();

        assert_eq!("pydoc_data.topics", pydoc_data_topics.name());
        assert_eq!(pydoc_data_topics.search_path(), &stdlib);
        assert_eq!(
            pydoc_data_topics.file().path(&db),
            &stdlib.join("pydoc_data/topics.pyi")
        );
    }

    #[test]
    fn resolve_package() {
        let TestCase { src, db, .. } = TestCaseBuilder::new()
            .with_src_files(&[("foo/__init__.py", "print('Hello, world!'")])
            .build();

        let foo_path = src.join("foo/__init__.py");
        let foo_module = resolve_module(&db, ModuleName::new_static("foo").unwrap()).unwrap();

        assert_eq!("foo", foo_module.name());
        assert_eq!(&src, foo_module.search_path());
        assert_eq!(&foo_path, foo_module.file().path(&db));

        assert_eq!(
            Some(&foo_module),
            path_to_module(&db, &FilePath::System(foo_path)).as_ref()
        );

        // Resolving by directory doesn't resolve to the init file.
        assert_eq!(
            None,
            path_to_module(&db, &FilePath::System(src.join("foo")))
        );
    }

    #[test]
    fn package_priority_over_module() {
        const SRC: &[FileSpec] = &[
            ("foo/__init__.py", "print('Hello, world!')"),
            ("foo.py", "print('Hello, world!')"),
        ];

        let TestCase { db, src, .. } = TestCaseBuilder::new().with_src_files(SRC).build();

        let foo_module = resolve_module(&db, ModuleName::new_static("foo").unwrap()).unwrap();
        let foo_init_path = src.join("foo/__init__.py");

        assert_eq!(&src, foo_module.search_path());
        assert_eq!(&foo_init_path, foo_module.file().path(&db));
        assert_eq!(ModuleKind::Package, foo_module.kind());

        assert_eq!(
            Some(foo_module),
            path_to_module(&db, &FilePath::System(foo_init_path))
        );
        assert_eq!(
            None,
            path_to_module(&db, &FilePath::System(src.join("foo.py")))
        );
    }

    #[test]
    fn typing_stub_over_module() {
        const SRC: &[FileSpec] = &[("foo.py", "print('Hello, world!')"), ("foo.pyi", "x: int")];

        let TestCase { db, src, .. } = TestCaseBuilder::new().with_src_files(SRC).build();

        let foo = resolve_module(&db, ModuleName::new_static("foo").unwrap()).unwrap();
        let foo_stub = src.join("foo.pyi");

        assert_eq!(&src, foo.search_path());
        assert_eq!(&foo_stub, foo.file().path(&db));

        assert_eq!(Some(foo), path_to_module(&db, &FilePath::System(foo_stub)));
        assert_eq!(
            None,
            path_to_module(&db, &FilePath::System(src.join("foo.py")))
        );
    }

    #[test]
    fn sub_packages() {
        const SRC: &[FileSpec] = &[
            ("foo/__init__.py", ""),
            ("foo/bar/__init__.py", ""),
            ("foo/bar/baz.py", "print('Hello, world!)'"),
        ];

        let TestCase { db, src, .. } = TestCaseBuilder::new().with_src_files(SRC).build();

        let baz_module =
            resolve_module(&db, ModuleName::new_static("foo.bar.baz").unwrap()).unwrap();
        let baz_path = src.join("foo/bar/baz.py");

        assert_eq!(&src, baz_module.search_path());
        assert_eq!(&baz_path, baz_module.file().path(&db));

        assert_eq!(
            Some(baz_module),
            path_to_module(&db, &FilePath::System(baz_path))
        );
    }

    #[test]
    fn namespace_package() {
        // From [PEP420](https://peps.python.org/pep-0420/#nested-namespace-packages).
        // But uses `src` for `project1` and `site-packages` for `project2`.
        // ```
        // src
        //   parent
        //     child
        //       one.py
        // site_packages
        //   parent
        //     child
        //       two.py
        // ```
        let TestCase {
            db,
            src,
            site_packages,
            ..
        } = TestCaseBuilder::new()
            .with_src_files(&[("parent/child/one.py", "print('Hello, world!')")])
            .with_site_packages_files(&[("parent/child/two.py", "print('Hello, world!')")])
            .build();

        let one_module_name = ModuleName::new_static("parent.child.one").unwrap();
        let one_module_path = FilePath::System(src.join("parent/child/one.py"));
        assert_eq!(
            resolve_module(&db, one_module_name),
            path_to_module(&db, &one_module_path)
        );

        let two_module_name = ModuleName::new_static("parent.child.two").unwrap();
        let two_module_path = FilePath::System(site_packages.join("parent/child/two.py"));
        assert_eq!(
            resolve_module(&db, two_module_name),
            path_to_module(&db, &two_module_path)
        );
    }

    #[test]
    fn regular_package_in_namespace_package() {
        // Adopted test case from the [PEP420 examples](https://peps.python.org/pep-0420/#nested-namespace-packages).
        // The `src/parent/child` package is a regular package. Therefore, `site_packages/parent/child/two.py` should not be resolved.
        // ```
        // src
        //   parent
        //     child
        //       one.py
        // site_packages
        //   parent
        //     child
        //       two.py
        // ```
        const SRC: &[FileSpec] = &[
            ("parent/child/__init__.py", "print('Hello, world!')"),
            ("parent/child/one.py", "print('Hello, world!')"),
        ];

        const SITE_PACKAGES: &[FileSpec] = &[("parent/child/two.py", "print('Hello, world!')")];

        let TestCase { db, src, .. } = TestCaseBuilder::new()
            .with_src_files(SRC)
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        let one_module_path = FilePath::System(src.join("parent/child/one.py"));
        let one_module_name =
            resolve_module(&db, ModuleName::new_static("parent.child.one").unwrap());
        assert_eq!(one_module_name, path_to_module(&db, &one_module_path));

        assert_eq!(
            None,
            resolve_module(&db, ModuleName::new_static("parent.child.two").unwrap())
        );
    }

    #[test]
    fn module_search_path_priority() {
        let TestCase {
            db,
            src,
            site_packages,
            ..
        } = TestCaseBuilder::new()
            .with_src_files(&[("foo.py", "")])
            .with_site_packages_files(&[("foo.py", "")])
            .build();

        let foo_module = resolve_module(&db, ModuleName::new_static("foo").unwrap()).unwrap();
        let foo_src_path = src.join("foo.py");

        assert_eq!(&src, foo_module.search_path());
        assert_eq!(&foo_src_path, foo_module.file().path(&db));
        assert_eq!(
            Some(foo_module),
            path_to_module(&db, &FilePath::System(foo_src_path))
        );

        assert_eq!(
            None,
            path_to_module(&db, &FilePath::System(site_packages.join("foo.py")))
        );
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn symlink() -> anyhow::Result<()> {
        use anyhow::Context;

        use ruff_db::program::RawProgramSettings;
        use ruff_db::system::{OsSystem, SystemPath};

        use crate::db::tests::TestDb;
        use crate::settings_resolution::program_from_raw_settings;

        let mut db = TestDb::new();

        let temp_dir = tempfile::tempdir()?;
        let root = temp_dir
            .path()
            .canonicalize()
            .context("Failed to canonicalize temp dir")?;
        let root = SystemPath::from_std_path(&root).unwrap();
        db.use_system(OsSystem::new(root));

        let src = root.join("src");
        let site_packages = root.join("site-packages");
        let custom_typeshed = root.join("typeshed");

        let foo = src.join("foo.py");
        let bar = src.join("bar.py");

        std::fs::create_dir_all(src.as_std_path())?;
        std::fs::create_dir_all(site_packages.as_std_path())?;
        std::fs::create_dir_all(custom_typeshed.join("stdlib").as_std_path())?;
        std::fs::File::create(custom_typeshed.join("stdlib/VERSIONS").as_std_path())?;

        std::fs::write(foo.as_std_path(), "")?;
        std::os::unix::fs::symlink(foo.as_std_path(), bar.as_std_path())?;

        program_from_raw_settings(
            &db,
            RawProgramSettings {
                target_version: TargetVersion::default(),
                search_paths: RawSearchPathSettings {
                    extra_paths: vec![],
                    src_root: src.clone(),
                    custom_typeshed: Some(custom_typeshed.clone()),
                    site_packages: vec![site_packages],
                },
            },
        )
        .unwrap();

        let foo_module = resolve_module(&db, ModuleName::new_static("foo").unwrap()).unwrap();
        let bar_module = resolve_module(&db, ModuleName::new_static("bar").unwrap()).unwrap();

        assert_ne!(foo_module, bar_module);

        assert_eq!(&src, foo_module.search_path());
        assert_eq!(&foo, foo_module.file().path(&db));

        // `foo` and `bar` shouldn't resolve to the same file

        assert_eq!(&src, bar_module.search_path());
        assert_eq!(&bar, bar_module.file().path(&db));
        assert_eq!(&foo, foo_module.file().path(&db));

        assert_ne!(&foo_module, &bar_module);

        assert_eq!(
            Some(foo_module),
            path_to_module(&db, &FilePath::System(foo))
        );
        assert_eq!(
            Some(bar_module),
            path_to_module(&db, &FilePath::System(bar))
        );

        Ok(())
    }

    #[test]
    fn deleting_an_unrelated_file_doesnt_change_module_resolution() {
        let TestCase { mut db, src, .. } = TestCaseBuilder::new()
            .with_src_files(&[("foo.py", "x = 1"), ("bar.py", "x = 2")])
            .with_target_version(TargetVersion::Py38)
            .build();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();

        let bar_path = src.join("bar.py");
        let bar = system_path_to_file(&db, &bar_path).expect("bar.py to exist");

        db.clear_salsa_events();

        // Delete `bar.py`
        db.memory_file_system().remove_file(&bar_path).unwrap();
        bar.sync(&mut db);

        // Re-query the foo module. The foo module should still be cached because `bar.py` isn't relevant
        // for resolving `foo`.

        let foo_module2 = resolve_module(&db, foo_module_name);

        assert!(!db
            .take_salsa_events()
            .iter()
            .any(|event| { matches!(event.kind, salsa::EventKind::WillExecute { .. }) }));

        assert_eq!(Some(foo_module), foo_module2);
    }

    #[test]
    fn adding_file_on_which_module_resolution_depends_invalidates_previously_failing_query_that_now_succeeds(
    ) -> anyhow::Result<()> {
        let TestCase { mut db, src, .. } = TestCaseBuilder::new().build();
        let foo_path = src.join("foo.py");

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        assert_eq!(resolve_module(&db, foo_module_name.clone()), None);

        // Now write the foo file
        db.write_file(&foo_path, "x = 1")?;

        let foo_file = system_path_to_file(&db, &foo_path).expect("foo.py to exist");

        let foo_module = resolve_module(&db, foo_module_name).expect("Foo module to resolve");
        assert_eq!(foo_file, foo_module.file());

        Ok(())
    }

    #[test]
    fn removing_file_on_which_module_resolution_depends_invalidates_previously_successful_query_that_now_fails(
    ) -> anyhow::Result<()> {
        const SRC: &[FileSpec] = &[("foo.py", "x = 1"), ("foo/__init__.py", "x = 2")];

        let TestCase { mut db, src, .. } = TestCaseBuilder::new().with_src_files(SRC).build();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).expect("foo module to exist");
        let foo_init_path = src.join("foo/__init__.py");

        assert_eq!(&foo_init_path, foo_module.file().path(&db));

        // Delete `foo/__init__.py` and the `foo` folder. `foo` should now resolve to `foo.py`
        db.memory_file_system().remove_file(&foo_init_path)?;
        db.memory_file_system()
            .remove_directory(foo_init_path.parent().unwrap())?;
        File::sync_path(&mut db, &foo_init_path);
        File::sync_path(&mut db, foo_init_path.parent().unwrap());

        let foo_module = resolve_module(&db, foo_module_name).expect("Foo module to resolve");
        assert_eq!(&src.join("foo.py"), foo_module.file().path(&db));

        Ok(())
    }

    #[test]
    fn adding_file_to_search_path_with_lower_priority_does_not_invalidate_query() {
        const TYPESHED: MockedTypeshed = MockedTypeshed {
            versions: "functools: 3.8-",
            stdlib_files: &[("functools.pyi", "def update_wrapper(): ...")],
        };

        let TestCase {
            mut db,
            stdlib,
            site_packages,
            ..
        } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let functools_module_name = ModuleName::new_static("functools").unwrap();
        let stdlib_functools_path = stdlib.join("functools.pyi");

        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        assert_eq!(functools_module.search_path(), &stdlib);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, &stdlib_functools_path)
        );

        // Adding a file to site-packages does not invalidate the query,
        // since site-packages takes lower priority in the module resolution
        db.clear_salsa_events();
        let site_packages_functools_path = site_packages.join("functools.py");
        db.write_file(&site_packages_functools_path, "f: int")
            .unwrap();
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        let events = db.take_salsa_events();
        assert_function_query_was_not_run(
            &db,
            resolve_module_query,
            ModuleNameIngredient::new(&db, functools_module_name.clone()),
            &events,
        );
        assert_eq!(functools_module.search_path(), &stdlib);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, &stdlib_functools_path)
        );
    }

    #[test]
    fn adding_file_to_search_path_with_higher_priority_invalidates_the_query() {
        const TYPESHED: MockedTypeshed = MockedTypeshed {
            versions: "functools: 3.8-",
            stdlib_files: &[("functools.pyi", "def update_wrapper(): ...")],
        };

        let TestCase {
            mut db,
            stdlib,
            src,
            ..
        } = TestCaseBuilder::new()
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let functools_module_name = ModuleName::new_static("functools").unwrap();
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        assert_eq!(functools_module.search_path(), &stdlib);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, stdlib.join("functools.pyi"))
        );

        // Adding a first-party file invalidates the query,
        // since first-party files take higher priority in module resolution:
        let src_functools_path = src.join("functools.py");
        db.write_file(&src_functools_path, "FOO: int").unwrap();
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        assert_eq!(functools_module.search_path(), &src);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, &src_functools_path)
        );
    }

    #[test]
    fn deleting_file_from_higher_priority_search_path_invalidates_the_query() {
        const SRC: &[FileSpec] = &[("functools.py", "FOO: int")];

        const TYPESHED: MockedTypeshed = MockedTypeshed {
            versions: "functools: 3.8-",
            stdlib_files: &[("functools.pyi", "def update_wrapper(): ...")],
        };

        let TestCase {
            mut db,
            stdlib,
            src,
            ..
        } = TestCaseBuilder::new()
            .with_src_files(SRC)
            .with_custom_typeshed(TYPESHED)
            .with_target_version(TargetVersion::Py38)
            .build();

        let functools_module_name = ModuleName::new_static("functools").unwrap();
        let src_functools_path = src.join("functools.py");

        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        assert_eq!(functools_module.search_path(), &src);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, &src_functools_path)
        );

        // If we now delete the first-party file,
        // it should resolve to the stdlib:
        db.memory_file_system()
            .remove_file(&src_functools_path)
            .unwrap();
        File::sync_path(&mut db, &src_functools_path);
        let functools_module = resolve_module(&db, functools_module_name.clone()).unwrap();
        assert_eq!(functools_module.search_path(), &stdlib);
        assert_eq!(
            Ok(functools_module.file()),
            system_path_to_file(&db, stdlib.join("functools.pyi"))
        );
    }

    #[test]
    fn editable_install_absolute_path() {
        const SITE_PACKAGES: &[FileSpec] = &[("_foo.pth", "/x/src")];
        let x_directory = [("/x/src/foo/__init__.py", ""), ("/x/src/foo/bar.py", "")];

        let TestCase { mut db, .. } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(x_directory).unwrap();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_bar_module_name = ModuleName::new_static("foo.bar").unwrap();

        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();
        let foo_bar_module = resolve_module(&db, foo_bar_module_name.clone()).unwrap();

        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::system("/x/src/foo/__init__.py")
        );
        assert_eq!(
            foo_bar_module.file().path(&db),
            &FilePath::system("/x/src/foo/bar.py")
        );
    }

    #[test]
    fn editable_install_pth_file_with_whitespace() {
        const SITE_PACKAGES: &[FileSpec] = &[
            ("_foo.pth", "        /x/src"),
            ("_bar.pth", "/y/src        "),
        ];
        let external_files = [("/x/src/foo.py", ""), ("/y/src/bar.py", "")];

        let TestCase { mut db, .. } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(external_files).unwrap();

        // Lines with leading whitespace in `.pth` files do not parse:
        let foo_module_name = ModuleName::new_static("foo").unwrap();
        assert_eq!(resolve_module(&db, foo_module_name), None);

        // Lines with trailing whitespace in `.pth` files do:
        let bar_module_name = ModuleName::new_static("bar").unwrap();
        let bar_module = resolve_module(&db, bar_module_name.clone()).unwrap();
        assert_eq!(
            bar_module.file().path(&db),
            &FilePath::system("/y/src/bar.py")
        );
    }

    #[test]
    fn editable_install_relative_path() {
        const SITE_PACKAGES: &[FileSpec] = &[
            ("_foo.pth", "../../x/../x/y/src"),
            ("../x/y/src/foo.pyi", ""),
        ];

        let TestCase { db, .. } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();

        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::system("/x/y/src/foo.pyi")
        );
    }

    #[test]
    fn editable_install_multiple_pth_files_with_multiple_paths() {
        const COMPLEX_PTH_FILE: &str = "\
/

# a comment
/baz

import not_an_editable_install; do_something_else_crazy_dynamic()

# another comment
spam

not_a_directory
";

        const SITE_PACKAGES: &[FileSpec] = &[
            ("_foo.pth", "../../x/../x/y/src"),
            ("_lots_of_others.pth", COMPLEX_PTH_FILE),
            ("../x/y/src/foo.pyi", ""),
            ("spam/spam.py", ""),
        ];

        let root_files = [("/a.py", ""), ("/baz/b.py", "")];

        let TestCase {
            mut db,
            site_packages,
            ..
        } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(root_files).unwrap();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let a_module_name = ModuleName::new_static("a").unwrap();
        let b_module_name = ModuleName::new_static("b").unwrap();
        let spam_module_name = ModuleName::new_static("spam").unwrap();

        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();
        let a_module = resolve_module(&db, a_module_name.clone()).unwrap();
        let b_module = resolve_module(&db, b_module_name.clone()).unwrap();
        let spam_module = resolve_module(&db, spam_module_name.clone()).unwrap();

        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::system("/x/y/src/foo.pyi")
        );
        assert_eq!(a_module.file().path(&db), &FilePath::system("/a.py"));
        assert_eq!(b_module.file().path(&db), &FilePath::system("/baz/b.py"));
        assert_eq!(
            spam_module.file().path(&db),
            &FilePath::System(site_packages.join("spam/spam.py"))
        );
    }

    #[test]
    fn module_resolution_paths_cached_between_different_module_resolutions() {
        const SITE_PACKAGES: &[FileSpec] = &[("_foo.pth", "/x/src"), ("_bar.pth", "/y/src")];
        let external_directories = [("/x/src/foo.py", ""), ("/y/src/bar.py", "")];

        let TestCase { mut db, .. } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(external_directories).unwrap();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let bar_module_name = ModuleName::new_static("bar").unwrap();

        let foo_module = resolve_module(&db, foo_module_name).unwrap();
        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::system("/x/src/foo.py")
        );

        db.clear_salsa_events();
        let bar_module = resolve_module(&db, bar_module_name).unwrap();
        assert_eq!(
            bar_module.file().path(&db),
            &FilePath::system("/y/src/bar.py")
        );
        let events = db.take_salsa_events();
        assert_const_function_query_was_not_run(&db, dynamic_resolution_paths, &events);
    }

    #[test]
    fn deleting_pth_file_on_which_module_resolution_depends_invalidates_cache() {
        const SITE_PACKAGES: &[FileSpec] = &[("_foo.pth", "/x/src")];
        let x_directory = [("/x/src/foo.py", "")];

        let TestCase {
            mut db,
            site_packages,
            ..
        } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(x_directory).unwrap();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();
        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::system("/x/src/foo.py")
        );

        db.memory_file_system()
            .remove_file(site_packages.join("_foo.pth"))
            .unwrap();

        File::sync_path(&mut db, &site_packages.join("_foo.pth"));

        assert_eq!(resolve_module(&db, foo_module_name.clone()), None);
    }

    #[test]
    fn deleting_editable_install_on_which_module_resolution_depends_invalidates_cache() {
        const SITE_PACKAGES: &[FileSpec] = &[("_foo.pth", "/x/src")];
        let x_directory = [("/x/src/foo.py", "")];

        let TestCase { mut db, .. } = TestCaseBuilder::new()
            .with_site_packages_files(SITE_PACKAGES)
            .build();

        db.write_files(x_directory).unwrap();

        let foo_module_name = ModuleName::new_static("foo").unwrap();
        let foo_module = resolve_module(&db, foo_module_name.clone()).unwrap();
        let src_path = SystemPathBuf::from("/x/src");
        assert_eq!(
            foo_module.file().path(&db),
            &FilePath::System(src_path.join("foo.py"))
        );

        db.memory_file_system()
            .remove_file(src_path.join("foo.py"))
            .unwrap();
        db.memory_file_system().remove_directory(&src_path).unwrap();
        File::sync_path(&mut db, &src_path.join("foo.py"));
        File::sync_path(&mut db, &src_path);
        assert_eq!(resolve_module(&db, foo_module_name.clone()), None);
    }

    #[test]
    fn no_duplicate_search_paths_added() {
        let TestCase { db, .. } = TestCaseBuilder::new()
            .with_src_files(&[("foo.py", "")])
            .with_site_packages_files(&[("_foo.pth", "/src")])
            .build();

        let search_paths: Vec<SearchPath> = module_search_paths(&db).map(Cow::into_owned).collect();

        assert!(search_paths
            .contains(&SearchPath::first_party(db.system(), SystemPathBuf::from("/src")).unwrap()));
        assert!(!search_paths
            .contains(&SearchPath::editable(db.system(), SystemPathBuf::from("/src")).unwrap()));
    }

    #[test]
    fn multiple_site_packages_with_editables() {
        let mut db = TestDb::new();

        let venv_site_packages = SystemPathBuf::from("/venv-site-packages");
        let site_packages_pth = venv_site_packages.join("foo.pth");
        let system_site_packages = SystemPathBuf::from("/system-site-packages");
        let editable_install_location = SystemPathBuf::from("/x/y/a.py");
        let system_site_packages_location = system_site_packages.join("a.py");

        db.memory_file_system()
            .create_directory_all("/src")
            .unwrap();
        db.write_files([
            (&site_packages_pth, "/x/y"),
            (&editable_install_location, ""),
            (&system_site_packages_location, ""),
        ])
        .unwrap();

        program_from_raw_settings(
            &db,
            RawProgramSettings {
                target_version: TargetVersion::default(),
                search_paths: RawSearchPathSettings {
                    extra_paths: vec![],
                    src_root: SystemPathBuf::from("/src"),
                    custom_typeshed: None,
                    site_packages: vec![venv_site_packages, system_site_packages],
                },
            },
        )
        .unwrap();

        // The editable installs discovered from the `.pth` file in the first `site-packages` directory
        // take precedence over the second `site-packages` directory...
        let a_module_name = ModuleName::new_static("a").unwrap();
        let a_module = resolve_module(&db, a_module_name.clone()).unwrap();
        assert_eq!(a_module.file().path(&db), &editable_install_location);

        db.memory_file_system()
            .remove_file(&site_packages_pth)
            .unwrap();
        File::sync_path(&mut db, &site_packages_pth);

        // ...But now that the `.pth` file in the first `site-packages` directory has been deleted,
        // the editable install no longer exists, so the module now resolves to the file in the
        // second `site-packages` directory
        let a_module = resolve_module(&db, a_module_name).unwrap();
        assert_eq!(a_module.file().path(&db), &system_site_packages_location);
    }
}
