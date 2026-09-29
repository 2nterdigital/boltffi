use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, Fields, ImplItem, Item, Lit, Meta, MetaList, MetaNameValue, Path, Token,
    TraitItem,
};

use crate::ScanError;

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct ActiveCfg {
    names: BTreeSet<String>,
    values: BTreeMap<String, BTreeSet<String>>,
    features: BTreeSet<String>,
}

impl ActiveCfg {
    pub fn from_cargo_env() -> Self {
        std::env::vars().fold(Self::default(), |mut active, (name, value)| {
            active.observe_cargo_env(&name, &value);
            active
        })
    }

    pub fn with_feature(mut self, feature: impl AsRef<str>) -> Self {
        self.features.insert(Self::feature_name(feature.as_ref()));
        self
    }

    pub fn with_features(mut self, features: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.features.extend(
            features
                .into_iter()
                .map(|feature| Self::feature_name(feature.as_ref())),
        );
        self
    }

    pub(crate) fn for_package(&self, features: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut package_cfg = self.clone();
        package_cfg.features = features
            .into_iter()
            .map(|feature| Self::feature_name(feature.as_ref()))
            .collect();
        package_cfg
    }

    pub fn with_name(mut self, name: impl AsRef<str>) -> Self {
        self.names.insert(Self::cfg_name(name.as_ref()));
        self
    }

    pub fn with_value(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.values
            .entry(Self::cfg_name(name.as_ref()))
            .or_default()
            .insert(value.into());
        self
    }

    /// Expands every `#[cfg_attr(predicate, attrs…)]` in `items` (fields,
    /// variants and impl / trait items included): an active one is replaced
    /// by its nested attributes, recursively; an inactive one is dropped.
    ///
    /// A BoltFFI marker behind a feature — `#[cfg_attr(feature = "boltffi",
    /// boltffi::data)]` — is then scanned exactly like the bare attribute,
    /// which is what lets a crate keep its FFI markers optional the way it
    /// would with `cfg_attr(feature = "uniffi", derive(uniffi::Record))`. A
    /// nested `cfg(...)` keeps gating the item as before.
    pub(crate) fn expand_cfg_attrs(&self, items: &mut [Item]) -> Result<(), ScanError> {
        items.iter_mut().try_for_each(|item| self.expand_item(item))
    }

    fn expand_item(&self, item: &mut Item) -> Result<(), ScanError> {
        match item {
            Item::Struct(item) => {
                self.expand_attrs(&mut item.attrs)?;
                self.expand_fields(&mut item.fields)
            }
            Item::Enum(item) => {
                self.expand_attrs(&mut item.attrs)?;
                item.variants.iter_mut().try_for_each(|variant| {
                    self.expand_attrs(&mut variant.attrs)?;
                    self.expand_fields(&mut variant.fields)
                })
            }
            Item::Impl(item) => {
                self.expand_attrs(&mut item.attrs)?;
                item.items.iter_mut().try_for_each(|item| match item {
                    ImplItem::Fn(item) => self.expand_attrs(&mut item.attrs),
                    ImplItem::Const(item) => self.expand_attrs(&mut item.attrs),
                    ImplItem::Type(item) => self.expand_attrs(&mut item.attrs),
                    ImplItem::Macro(item) => self.expand_attrs(&mut item.attrs),
                    _ => Ok(()),
                })
            }
            Item::Trait(item) => {
                self.expand_attrs(&mut item.attrs)?;
                item.items.iter_mut().try_for_each(|item| match item {
                    TraitItem::Fn(item) => self.expand_attrs(&mut item.attrs),
                    TraitItem::Const(item) => self.expand_attrs(&mut item.attrs),
                    TraitItem::Type(item) => self.expand_attrs(&mut item.attrs),
                    TraitItem::Macro(item) => self.expand_attrs(&mut item.attrs),
                    _ => Ok(()),
                })
            }
            Item::Mod(item) => {
                self.expand_attrs(&mut item.attrs)?;
                match &mut item.content {
                    Some((_, items)) => self.expand_cfg_attrs(items),
                    None => Ok(()),
                }
            }
            Item::Fn(item) => self.expand_attrs(&mut item.attrs),
            Item::Const(item) => self.expand_attrs(&mut item.attrs),
            Item::Static(item) => self.expand_attrs(&mut item.attrs),
            Item::Type(item) => self.expand_attrs(&mut item.attrs),
            Item::Macro(item) => self.expand_attrs(&mut item.attrs),
            Item::Use(item) => self.expand_attrs(&mut item.attrs),
            _ => Ok(()),
        }
    }

    fn expand_fields(&self, fields: &mut Fields) -> Result<(), ScanError> {
        fields
            .iter_mut()
            .try_for_each(|field| self.expand_attrs(&mut field.attrs))
    }

    fn expand_attrs(&self, attrs: &mut Vec<Attribute>) -> Result<(), ScanError> {
        if !attrs.iter().any(|attr| attr.path().is_ident("cfg_attr")) {
            return Ok(());
        }
        let mut expanded = Vec::with_capacity(attrs.len());
        for attr in attrs.drain(..) {
            self.expand_attr(attr, &mut expanded)?;
        }
        *attrs = expanded;
        Ok(())
    }

    fn expand_attr(&self, attr: Attribute, expanded: &mut Vec<Attribute>) -> Result<(), ScanError> {
        if !attr.path().is_ident("cfg_attr") {
            expanded.push(attr);
            return Ok(());
        }
        let Meta::List(list) = &attr.meta else {
            return Err(Self::invalid_attribute(attr.meta.to_token_stream()));
        };
        let metas = self.predicates(list)?;
        let (predicate, nested) = metas
            .split_first()
            .ok_or_else(|| Self::invalid_attribute(list.to_token_stream()))?;
        if !self.matches_meta(predicate)? {
            return Ok(());
        }
        nested.iter().try_for_each(|meta| {
            self.expand_attr(
                Attribute {
                    pound_token: attr.pound_token,
                    style: attr.style,
                    bracket_token: attr.bracket_token,
                    meta: meta.clone(),
                },
                expanded,
            )
        })
    }

    pub fn matches_attrs(&self, attrs: &[Attribute]) -> Result<bool, ScanError> {
        attrs
            .iter()
            .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
            .map(|attr| self.matches_attr(attr))
            .try_fold(true, |active, matches| {
                matches.map(|matches| active && matches)
            })
    }

    pub(crate) fn matches_item(&self, item: &syn::Item) -> Result<bool, ScanError> {
        self.matches_attrs(Self::item_attrs(item))
    }

    pub(crate) fn retain_active_impl_items(
        &self,
        item: &mut syn::ItemImpl,
    ) -> Result<(), ScanError> {
        item.items = std::mem::take(&mut item.items)
            .into_iter()
            .map(|item| {
                self.matches_attrs(Self::impl_item_attrs(&item))
                    .map(|active| (active, item))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(active, item)| active.then_some(item))
            .collect();
        Ok(())
    }

    pub(crate) fn retain_active_enum_variants(
        &self,
        item: &mut syn::ItemEnum,
    ) -> Result<(), ScanError> {
        item.variants = std::mem::take(&mut item.variants)
            .into_iter()
            .map(|variant| {
                self.matches_attrs(&variant.attrs)
                    .map(|active| (active, variant))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(active, variant)| active.then_some(variant))
            .collect();
        item.variants
            .iter_mut()
            .try_for_each(|variant| self.retain_active_fields(&mut variant.fields))?;
        Ok(())
    }

    pub(crate) fn retain_active_struct_fields(
        &self,
        item: &mut syn::ItemStruct,
    ) -> Result<(), ScanError> {
        self.retain_active_fields(&mut item.fields)
    }

    fn retain_active_fields(&self, fields: &mut syn::Fields) -> Result<(), ScanError> {
        let fields = match fields {
            syn::Fields::Named(fields) => &mut fields.named,
            syn::Fields::Unnamed(fields) => &mut fields.unnamed,
            syn::Fields::Unit => return Ok(()),
        };
        *fields = std::mem::take(fields)
            .into_iter()
            .map(|field| {
                self.matches_attrs(&field.attrs)
                    .map(|active| (active, field))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(active, field)| active.then_some(field))
            .collect();
        Ok(())
    }

    fn observe_cargo_env(&mut self, name: &str, value: &str) {
        if let Some(feature) = name.strip_prefix("CARGO_FEATURE_") {
            self.features.insert(Self::feature_name(feature));
            return;
        }

        if let Some(name) = name.strip_prefix("CARGO_CFG_") {
            let name = Self::cfg_name(name);
            if value.is_empty() {
                self.names.insert(name);
                return;
            }

            value
                .split(',')
                .filter(|value| !value.is_empty())
                .for_each(|value| {
                    if name == "feature" {
                        self.features.insert(Self::feature_name(value));
                    }
                    self.values
                        .entry(name.clone())
                        .or_default()
                        .insert(value.to_owned());
                });
        }
    }

    fn matches_attr(&self, attr: &Attribute) -> Result<bool, ScanError> {
        if attr.path().is_ident("cfg_attr") {
            let Meta::List(list) = &attr.meta else {
                return Err(Self::invalid_attribute(attr.meta.to_token_stream()));
            };
            return self.matches_cfg_attr(list);
        }

        attr.parse_args::<Meta>()
            .map_err(|_| Self::invalid_attribute(attr.meta.to_token_stream()))
            .and_then(|meta| self.matches_meta(&meta))
    }

    fn matches_meta(&self, meta: &Meta) -> Result<bool, ScanError> {
        match meta {
            Meta::Path(path) => Ok(self.matches_name(path)),
            Meta::NameValue(value) => self.matches_value(value),
            Meta::List(list) if list.path.is_ident("all") => self.matches_all(list),
            Meta::List(list) if list.path.is_ident("any") => self.matches_any(list),
            Meta::List(list) if list.path.is_ident("not") => self.matches_not(list),
            Meta::List(list) => Err(Self::invalid_attribute(list.to_token_stream())),
        }
    }

    fn matches_name(&self, path: &Path) -> bool {
        path.get_ident()
            .map(|ident| self.names.contains(&Self::cfg_name(&ident.to_string())))
            .unwrap_or(false)
    }

    fn matches_value(&self, value: &MetaNameValue) -> Result<bool, ScanError> {
        let Some(name) = value.path.get_ident().map(ToString::to_string) else {
            return Ok(false);
        };
        let Some(value) = Self::string_value(&value.value) else {
            return Err(Self::invalid_attribute(value.to_token_stream()));
        };

        if name == "feature" {
            return Ok(self.features.contains(&Self::feature_name(&value)));
        }

        Ok(self
            .values
            .get(&Self::cfg_name(&name))
            .is_some_and(|values| values.contains(&value)))
    }

    fn matches_all(&self, list: &MetaList) -> Result<bool, ScanError> {
        self.predicates(list)?
            .iter()
            .map(|meta| self.matches_meta(meta))
            .try_fold(true, |active, matches| {
                matches.map(|matches| active && matches)
            })
    }

    fn matches_any(&self, list: &MetaList) -> Result<bool, ScanError> {
        self.predicates(list)?
            .iter()
            .map(|meta| self.matches_meta(meta))
            .try_fold(false, |active, matches| {
                matches.map(|matches| active || matches)
            })
    }

    fn matches_not(&self, list: &MetaList) -> Result<bool, ScanError> {
        let predicates = self.predicates(list)?;
        match predicates.len() {
            1 => self.matches_meta(&predicates[0]).map(|active| !active),
            _ => Err(Self::invalid_attribute(list.to_token_stream())),
        }
    }

    fn matches_cfg_attr(&self, list: &MetaList) -> Result<bool, ScanError> {
        let attributes = self.predicates(list)?;
        let mut attributes = attributes.iter();
        let predicate = attributes
            .next()
            .ok_or_else(|| Self::invalid_attribute(list.to_token_stream()))?;
        if !self.matches_meta(predicate)? {
            return Ok(true);
        }

        attributes
            .map(|attribute| self.matches_nested_attr(attribute))
            .try_fold(true, |active, matches| {
                matches.map(|matches| active && matches)
            })
    }

    fn matches_nested_attr(&self, attribute: &Meta) -> Result<bool, ScanError> {
        match attribute {
            Meta::List(list) if list.path.is_ident("cfg") => {
                let predicates = self.predicates(list)?;
                match predicates.as_slice() {
                    [predicate] => self.matches_meta(predicate),
                    _ => Err(Self::invalid_attribute(list.to_token_stream())),
                }
            }
            Meta::List(list) if list.path.is_ident("cfg_attr") => self.matches_cfg_attr(list),
            _ => Ok(true),
        }
    }

    fn predicates(&self, list: &MetaList) -> Result<Vec<Meta>, ScanError> {
        Punctuated::<Meta, Token![,]>::parse_terminated
            .parse2(list.tokens.clone())
            .map(|items| items.into_iter().collect())
            .map_err(|_| Self::invalid_attribute(list.to_token_stream()))
    }

    fn string_value(value: &Expr) -> Option<String> {
        match value {
            Expr::Lit(value) => match &value.lit {
                Lit::Str(value) => Some(value.value()),
                _ => None,
            },
            _ => None,
        }
    }

    fn feature_name(feature: &str) -> String {
        feature.replace('-', "_").to_ascii_uppercase()
    }

    fn cfg_name(name: &str) -> String {
        name.to_ascii_lowercase()
    }

    fn invalid_attribute(tokens: TokenStream) -> ScanError {
        ScanError::InvalidAttribute {
            attribute: tokens.to_string(),
        }
    }

    fn item_attrs(item: &syn::Item) -> &[Attribute] {
        match item {
            syn::Item::Const(item) => &item.attrs,
            syn::Item::Enum(item) => &item.attrs,
            syn::Item::ExternCrate(item) => &item.attrs,
            syn::Item::Fn(item) => &item.attrs,
            syn::Item::ForeignMod(item) => &item.attrs,
            syn::Item::Impl(item) => &item.attrs,
            syn::Item::Macro(item) => &item.attrs,
            syn::Item::Mod(item) => &item.attrs,
            syn::Item::Static(item) => &item.attrs,
            syn::Item::Struct(item) => &item.attrs,
            syn::Item::Trait(item) => &item.attrs,
            syn::Item::TraitAlias(item) => &item.attrs,
            syn::Item::Type(item) => &item.attrs,
            syn::Item::Union(item) => &item.attrs,
            syn::Item::Use(item) => &item.attrs,
            _ => &[],
        }
    }

    fn impl_item_attrs(item: &syn::ImplItem) -> &[Attribute] {
        match item {
            syn::ImplItem::Const(item) => &item.attrs,
            syn::ImplItem::Fn(item) => &item.attrs,
            syn::ImplItem::Macro(item) => &item.attrs,
            syn::ImplItem::Type(item) => &item.attrs,
            _ => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ActiveCfg;

    fn matches(active: &ActiveCfg, source: &str) -> bool {
        let source = format!("{source} struct Demo;");
        let file = syn::parse_str::<syn::File>(&source).expect("valid item");
        let syn::Item::Struct(item) = &file.items[0] else {
            panic!("expected struct");
        };
        active.matches_attrs(&item.attrs).expect("cfg evaluation")
    }

    #[test]
    fn feature_cfg_uses_cargo_feature_normalization() {
        let active = ActiveCfg::default().with_feature("native_ffi");

        assert!(matches(&active, "#[cfg(feature = \"native-ffi\")]"));
    }

    #[test]
    fn package_features_replace_root_features_without_losing_target_cfg() {
        let root = ActiveCfg::default()
            .with_feature("root-only")
            .with_name("unix");
        let dependency = root.for_package(["dependency-only"]);

        assert!(matches(&root, "#[cfg(feature = \"root-only\")]"));
        assert!(!matches(&dependency, "#[cfg(feature = \"root-only\")]"));
        assert!(matches(
            &dependency,
            "#[cfg(feature = \"dependency-only\")]"
        ));
        assert!(matches(&dependency, "#[cfg(unix)]"));
    }

    #[test]
    fn cfg_predicates_match_active_names_and_values() {
        let active = ActiveCfg::default()
            .with_name("unix")
            .with_value("target_os", "ios");

        assert!(matches(
            &active,
            "#[cfg(all(unix, any(target_os = \"ios\", target_os = \"macos\")))]"
        ));
        assert!(!matches(&active, "#[cfg(not(unix))]"));
    }

    #[test]
    fn cfg_attr_applies_nested_cfg_only_when_its_predicate_matches() {
        let inactive = ActiveCfg::default();
        let active = ActiveCfg::default().with_feature("ffi");
        let source = "#[cfg_attr(not(feature = \"ffi\"), cfg(any()))]";

        assert!(!matches(&inactive, source));
        assert!(matches(&active, source));
    }

    fn expanded(active: &ActiveCfg, source: &str) -> syn::File {
        let mut file = syn::parse_str::<syn::File>(source).expect("valid items");
        active
            .expand_cfg_attrs(&mut file.items)
            .expect("cfg_attr expansion");
        file
    }

    fn attr_paths(attrs: &[syn::Attribute]) -> Vec<String> {
        attrs
            .iter()
            .map(|attr| {
                quote::ToTokens::to_token_stream(attr.path())
                    .to_string()
                    .replace(' ', "")
            })
            .collect()
    }

    #[test]
    fn active_cfg_attr_marker_becomes_the_bare_attribute() {
        let source = "#[derive(Clone)] #[cfg_attr(feature = \"boltffi\", boltffi::data, repr(i32))] enum Mode { A }";
        let file = expanded(&ActiveCfg::default().with_feature("boltffi"), source);
        let syn::Item::Enum(item) = &file.items[0] else {
            panic!("expected enum")
        };
        assert_eq!(
            attr_paths(&item.attrs),
            vec!["derive", "boltffi::data", "repr"]
        );
    }

    #[test]
    fn inactive_cfg_attr_marker_is_dropped() {
        let source = "#[cfg_attr(feature = \"boltffi\", boltffi::data)] struct Point { x: f64 }";
        let file = expanded(&ActiveCfg::default(), source);
        let syn::Item::Struct(item) = &file.items[0] else {
            panic!("expected struct")
        };
        assert!(item.attrs.is_empty());
    }

    #[test]
    fn cfg_attr_expands_inside_fields_variants_impls_and_inline_modules() {
        let source = "mod inner {\n\
            #[cfg_attr(feature = \"ffi\", boltffi::data)] enum E { #[cfg_attr(feature = \"ffi\", boltffi::skip)] A { #[cfg_attr(feature = \"ffi\", boltffi::skip)] x: u8 } }\n\
            #[cfg_attr(feature = \"ffi\", boltffi::export)] impl S { #[cfg_attr(feature = \"ffi\", boltffi::skip)] fn f(&self) {} }\n\
            }";
        let file = expanded(&ActiveCfg::default().with_feature("ffi"), source);
        let syn::Item::Mod(module) = &file.items[0] else {
            panic!("expected mod")
        };
        let items = &module.content.as_ref().expect("inline module").1;
        let syn::Item::Enum(item) = &items[0] else {
            panic!("expected enum")
        };
        assert_eq!(attr_paths(&item.attrs), vec!["boltffi::data"]);
        let variant = &item.variants[0];
        assert_eq!(attr_paths(&variant.attrs), vec!["boltffi::skip"]);
        assert_eq!(
            attr_paths(&variant.fields.iter().next().expect("field").attrs),
            vec!["boltffi::skip"]
        );
        let syn::Item::Impl(item) = &items[1] else {
            panic!("expected impl")
        };
        assert_eq!(attr_paths(&item.attrs), vec!["boltffi::export"]);
        let syn::ImplItem::Fn(method) = &item.items[0] else {
            panic!("expected fn")
        };
        assert_eq!(attr_paths(&method.attrs), vec!["boltffi::skip"]);
    }

    #[test]
    fn nested_cfg_attr_expands_to_a_cfg_that_still_gates_the_item() {
        let active = ActiveCfg::default().with_feature("ffi");
        let file = expanded(
            &active,
            "#[cfg_attr(feature = \"ffi\", cfg_attr(all(), cfg(any())))] struct Gone;",
        );
        let syn::Item::Struct(item) = &file.items[0] else {
            panic!("expected struct")
        };
        assert_eq!(attr_paths(&item.attrs), vec!["cfg"]);
        assert!(!active.matches_attrs(&item.attrs).expect("cfg evaluation"));
    }

    #[test]
    fn nested_cfg_attr_is_evaluated_recursively() {
        let source = "#[cfg_attr(all(), cfg_attr(all(), cfg(any())))]";

        assert!(!matches(&ActiveCfg::default(), source));
    }
}
