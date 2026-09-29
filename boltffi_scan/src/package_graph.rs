use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use syn::{Attribute, Item};
use walkdir::WalkDir;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PackageId(String);

#[derive(Clone, Debug)]
pub struct ExportedPackage {
    id: PackageId,
    source_file: PathBuf,
    module_name: String,
    resolved_features: ResolvedFeatures,
}

pub struct PackageGraph {
    packages: HashMap<PackageId, CargoPackage>,
    resolved_packages: HashMap<PackageId, ResolvedPackage>,
    root_id: PackageId,
}

#[derive(Clone, Debug)]
struct ResolvedFeatures(BTreeSet<String>);

struct ResolvedPackage {
    dependencies: Vec<CargoDependency>,
    features: ResolvedFeatures,
}

#[derive(Debug)]
pub struct LoadError {
    message: String,
}

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
    resolve: Option<CargoResolve>,
}

#[derive(Clone, Deserialize)]
struct CargoPackage {
    id: String,
    manifest_path: PathBuf,
    source: Option<String>,
    targets: Vec<CargoTarget>,
}

#[derive(Clone, Deserialize)]
struct CargoTarget {
    kind: Vec<String>,
    src_path: PathBuf,
}

#[derive(Deserialize)]
struct CargoResolve {
    nodes: Vec<CargoNode>,
}

#[derive(Deserialize)]
struct CargoNode {
    id: String,
    deps: Vec<CargoNodeDependency>,
    #[serde(default)]
    features: BTreeSet<String>,
}

#[derive(Deserialize)]
struct CargoNodeDependency {
    name: String,
    pkg: String,
}

impl PackageId {
    fn new(value: String) -> Self {
        Self(value)
    }
}

impl PackageGraph {
    pub fn load(manifest_dir: &Path) -> Result<Option<Self>, LoadError> {
        let manifest_path = manifest_dir.join("Cargo.toml");
        if !manifest_path.exists() {
            return Ok(None);
        }

        let metadata = MetadataCommand::new(&manifest_path).metadata()?;
        let root_id = Self::resolve_root_id(&metadata.packages, &manifest_path)?;
        Ok(Some(Self::from_metadata(metadata, root_id)))
    }

    pub fn root_id(&self) -> &PackageId {
        &self.root_id
    }

    pub fn reachable_exported_dependencies(&self, id: &PackageId) -> Vec<ExportedPackage> {
        self.collect_reachable_exported_dependencies(id, &mut HashSet::new())
    }

    pub fn direct_exported_dependencies(&self, id: &PackageId) -> Vec<ExportedPackage> {
        self.exported_dependencies(id)
    }

    fn from_metadata(metadata: CargoMetadata, root_id: PackageId) -> Self {
        let packages = metadata
            .packages
            .into_iter()
            .map(|package| (PackageId::new(package.id.clone()), package))
            .collect::<HashMap<_, _>>();
        let resolved_packages = metadata
            .resolve
            .map(|resolve| {
                resolve
                    .nodes
                    .into_iter()
                    .map(CargoNode::into_resolved)
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        Self {
            packages,
            resolved_packages,
            root_id,
        }
    }

    fn resolve_root_id(
        packages: &[CargoPackage],
        manifest_path: &Path,
    ) -> Result<PackageId, LoadError> {
        let canonical_manifest = manifest_path.canonicalize().map_err(|error| {
            LoadError::new(format!(
                "failed to canonicalize {}: {error}",
                manifest_path.display()
            ))
        })?;
        packages
            .iter()
            .find(|package| {
                package
                    .manifest_path
                    .canonicalize()
                    .is_ok_and(|path| path == canonical_manifest)
            })
            .map(|package| PackageId::new(package.id.clone()))
            .ok_or_else(|| {
                LoadError::new(format!(
                    "cargo metadata did not include package for {}",
                    manifest_path.display()
                ))
            })
    }

    fn exported_dependencies(&self, id: &PackageId) -> Vec<ExportedPackage> {
        self.resolved_packages
            .get(id)
            .into_iter()
            .flat_map(|resolved_package| resolved_package.dependencies.iter())
            .filter_map(|dependency| {
                let resolved_package = self.resolved_packages.get(&dependency.package_id)?;
                let package = self.packages.get(&dependency.package_id)?;
                package.is_local().then_some(())?;
                package.has_exports().then_some(())?;
                package.export(
                    dependency.package_id.clone(),
                    dependency.import_name.clone(),
                    resolved_package.features.clone(),
                )
            })
            .collect()
    }

    fn collect_reachable_exported_dependencies(
        &self,
        id: &PackageId,
        visited: &mut HashSet<PackageId>,
    ) -> Vec<ExportedPackage> {
        self.exported_dependencies(id)
            .into_iter()
            .flat_map(|package| {
                if !visited.insert(package.id.clone()) {
                    return Vec::new();
                }
                self.collect_reachable_exported_dependencies(package.id(), visited)
                    .into_iter()
                    .chain(std::iter::once(package))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

impl ExportedPackage {
    pub fn id(&self) -> &PackageId {
        &self.id
    }

    pub fn source_file(&self) -> &Path {
        &self.source_file
    }

    pub fn module_name(&self) -> &str {
        &self.module_name
    }

    pub fn resolved_features(&self) -> impl Iterator<Item = &str> {
        self.resolved_features.iter()
    }
}

impl ResolvedFeatures {
    fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl LoadError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LoadError {}

struct MetadataCommand<'manifest> {
    manifest_path: &'manifest Path,
}

impl<'manifest> MetadataCommand<'manifest> {
    fn new(manifest_path: &'manifest Path) -> Self {
        Self { manifest_path }
    }

    fn metadata(self) -> Result<CargoMetadata, LoadError> {
        let mut command = Command::new(Self::cargo_executable());
        command
            .args(["metadata", "--format-version", "1", "--manifest-path"])
            .arg(self.manifest_path)
            .args(Self::mode_args());
        let output = command
            .output()
            .map_err(|error| LoadError::new(format!("cargo metadata failed: {error}")))?;
        if !output.status.success() {
            return Err(LoadError::new(format!(
                "cargo metadata failed with status {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| LoadError::new(format!("failed to parse cargo metadata: {error}")))
    }

    fn cargo_executable() -> OsString {
        env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
    }

    fn mode_args() -> &'static [&'static str] {
        if Self::env_flag("CARGO_FROZEN") {
            &["--frozen"]
        } else if Self::env_flag("CARGO_NET_OFFLINE") || Self::env_flag("CARGO_OFFLINE") {
            &["--offline"]
        } else if Self::env_flag("CARGO_LOCKED") {
            &["--locked"]
        } else {
            &[]
        }
    }

    fn env_flag(name: &str) -> bool {
        env::var(name)
            .ok()
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
    }
}

#[derive(Clone)]
struct CargoDependency {
    import_name: String,
    package_id: PackageId,
}

impl From<CargoNodeDependency> for CargoDependency {
    fn from(dependency: CargoNodeDependency) -> Self {
        Self {
            import_name: cargo_crate_name(&dependency.name),
            package_id: PackageId::new(dependency.pkg),
        }
    }
}

impl CargoNode {
    fn into_resolved(self) -> (PackageId, ResolvedPackage) {
        let Self { id, deps, features } = self;
        (
            PackageId::new(id),
            ResolvedPackage {
                dependencies: deps.into_iter().map(CargoDependency::from).collect(),
                features: ResolvedFeatures(features),
            },
        )
    }
}

impl CargoPackage {
    fn is_local(&self) -> bool {
        self.source.is_none()
    }

    fn manifest_dir(&self) -> Option<PathBuf> {
        self.manifest_path.parent().map(Path::to_path_buf)
    }

    fn source_root(&self) -> Option<PathBuf> {
        self.targets
            .iter()
            .find(|target| target.is_library())
            .and_then(|target| target.src_path.parent())
            .map(Path::to_path_buf)
            .or_else(|| {
                self.manifest_dir()
                    .map(|manifest_dir| manifest_dir.join("src"))
            })
    }

    fn source_file(&self) -> Option<PathBuf> {
        self.targets
            .iter()
            .find(|target| target.is_library())
            .map(|target| target.src_path.clone())
            .or_else(|| {
                self.source_root()
                    .map(|source_root| source_root.join("lib.rs"))
            })
    }

    fn export(
        &self,
        id: PackageId,
        import_name: String,
        resolved_features: ResolvedFeatures,
    ) -> Option<ExportedPackage> {
        Some(ExportedPackage {
            id,
            source_file: self.source_file()?,
            module_name: import_name,
            resolved_features,
        })
    }

    fn has_exports(&self) -> bool {
        self.source_root().is_some_and(|source_root| {
            WalkDir::new(source_root)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "rs")
                })
                .map(|entry| entry.path().to_path_buf())
                .any(|path| {
                    fs::read_to_string(path)
                        .ok()
                        .and_then(|source| syn::parse_file(&source).ok())
                        .is_some_and(|syntax| ExportDetector::file_has_exports(&syntax))
                })
        })
    }
}

impl CargoTarget {
    fn is_library(&self) -> bool {
        self.kind.iter().any(|kind| {
            matches!(
                kind.as_str(),
                "lib" | "rlib" | "staticlib" | "cdylib" | "dylib"
            )
        })
    }
}

struct ExportDetector;

impl ExportDetector {
    fn file_has_exports(file: &syn::File) -> bool {
        file.items.iter().any(Self::item_has_exports)
    }

    fn item_has_exports(item: &Item) -> bool {
        match item {
            Item::Struct(item_struct) => {
                Self::has_any_attribute(&item_struct.attrs, &["ffi_record", "data", "error"])
                    || Self::has_ffi_type_derive(&item_struct.attrs)
            }
            Item::Enum(item_enum) => Self::has_any_attribute(&item_enum.attrs, &["data", "error"]),
            Item::Impl(item_impl) => Self::has_any_attribute(
                &item_impl.attrs,
                &["ffi_class", "export", "data", "custom_ffi"],
            ),
            Item::Trait(item_trait) => {
                Self::has_any_attribute(&item_trait.attrs, &["ffi_callback", "export"])
            }
            Item::Fn(item_fn) => Self::has_any_attribute(&item_fn.attrs, &["ffi_func", "export"]),
            Item::Const(item_const) => Self::has_any_attribute(&item_const.attrs, &["export"]),
            _ => false,
        }
    }

    /// Whether a marker in `names` is on the item — bare, or behind any
    /// `cfg_attr` (whether that predicate is active is decided later, when
    /// the package is scanned with its resolved features; here a crate whose
    /// markers are all feature-gated must still count as exporting).
    fn has_any_attribute(attrs: &[Attribute], names: &[&str]) -> bool {
        attrs
            .iter()
            .any(|attr| Self::meta_names_marker(&attr.meta, names))
    }

    fn meta_names_marker(meta: &syn::Meta, names: &[&str]) -> bool {
        if meta.path().is_ident("cfg_attr") {
            let syn::Meta::List(list) = meta else {
                return false;
            };
            return list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .is_ok_and(|metas| {
                    metas
                        .iter()
                        .skip(1)
                        .any(|nested| Self::meta_names_marker(nested, names))
                });
        }
        meta.path()
            .segments
            .last()
            .is_some_and(|segment| names.iter().any(|name| segment.ident == *name))
    }

    fn has_ffi_type_derive(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|attr| {
            if !attr.path().is_ident("derive") {
                return false;
            }
            attr.parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .ok()
            .is_some_and(|paths| {
                paths.iter().any(|path| {
                    path.segments
                        .last()
                        .is_some_and(|segment| segment.ident == "FfiType")
                })
            })
        })
    }
}

fn cargo_crate_name(package_or_target_name: &str) -> String {
    package_or_target_name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::ExportDetector;

    #[test]
    fn feature_gated_markers_count_as_exports() {
        let gated = syn::parse_str::<syn::File>(
            "#[cfg_attr(feature = \"ffi\", boltffi::data)] pub struct Point { pub x: f64 }",
        )
        .expect("valid item");
        assert!(ExportDetector::file_has_exports(&gated));

        let unrelated = syn::parse_str::<syn::File>(
            "#[cfg_attr(feature = \"ffi\", derive(Debug))] pub struct Point { pub x: f64 }",
        )
        .expect("valid item");
        assert!(!ExportDetector::file_has_exports(&unrelated));
    }
}
