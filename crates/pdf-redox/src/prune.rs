use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    content::{form_content, form_resources, page_content, page_resources, resolved_dictionary},
};
use flpdf::{DetachedResourceUsage, find_resources_detached};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

type ResourceNamesByType = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourcePruneStats {
    pub entries_removed: usize,
    pub font_entries_removed: usize,
    pub xobject_entries_removed: usize,
    pub ext_gstate_entries_removed: usize,
    pub pattern_entries_removed: usize,
    pub properties_entries_removed: usize,
    pub shading_entries_removed: usize,
}

impl ResourcePruneStats {
    fn record_category(&mut self, category: &[u8], count: usize) {
        self.entries_removed = self.entries_removed.saturating_add(count);
        let slot = match category {
            b"Font" => &mut self.font_entries_removed,
            b"XObject" => &mut self.xobject_entries_removed,
            b"ExtGState" => &mut self.ext_gstate_entries_removed,
            b"Pattern" => &mut self.pattern_entries_removed,
            b"Properties" => &mut self.properties_entries_removed,
            b"Shading" => &mut self.shading_entries_removed,
            _ => return,
        };
        *slot = slot.saturating_add(count);
    }

    fn merge(&mut self, other: Self) {
        self.entries_removed = self.entries_removed.saturating_add(other.entries_removed);
        self.font_entries_removed = self
            .font_entries_removed
            .saturating_add(other.font_entries_removed);
        self.xobject_entries_removed = self
            .xobject_entries_removed
            .saturating_add(other.xobject_entries_removed);
        self.ext_gstate_entries_removed = self
            .ext_gstate_entries_removed
            .saturating_add(other.ext_gstate_entries_removed);
        self.pattern_entries_removed = self
            .pattern_entries_removed
            .saturating_add(other.pattern_entries_removed);
        self.properties_entries_removed = self
            .properties_entries_removed
            .saturating_add(other.properties_entries_removed);
        self.shading_entries_removed = self
            .shading_entries_removed
            .saturating_add(other.shading_entries_removed);
    }
}

fn xobject_handles(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<Vec<(Vec<u8>, ObjectHandle)>> {
    let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, value) in xobjects {
        let OwnedObject::Reference(handle) = value else {
            continue;
        };
        out.push((name, handle));
    }
    Ok(out)
}

fn is_form(document: &EditDocument, handle: ObjectHandle) -> Result<bool> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    if !matches!(object, OwnedObject::Stream { .. }) {
        return Ok(false);
    }
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(
        matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Form"),
    )
}

fn scan(content: &[u8]) -> Option<DetachedResourceUsage> {
    find_resources_detached(content)
        .ok()
        .filter(|usage| !usage.pending_operands)
}

fn extend_names_by_type(target: &mut ResourceNamesByType, source: &ResourceNamesByType) {
    for (resource_type, names) in source {
        target
            .entry(resource_type.clone())
            .or_default()
            .extend(names.iter().cloned());
    }
}

fn borrowed_form_names(
    document: &EditDocument,
    resources: &OwnedDictionary,
    root_usage: &DetachedResourceUsage,
    used_by_type: &mut ResourceNamesByType,
) -> Result<BTreeSet<Vec<u8>>> {
    let mut used = BTreeSet::new();
    let mut pending = VecDeque::new();
    let mut seen = BTreeSet::new();
    let xobjects = xobject_handles(document, resources)?;
    if let Some(names) = root_usage.names_by_resource_type.get(b"XObject".as_slice()) {
        for (name, handle) in &xobjects {
            if names.contains(name) && is_form(document, *handle)? {
                pending.push_back(*handle);
            }
        }
    }
    while let Some(form) = pending.pop_front() {
        if !seen.insert(form) {
            continue;
        }
        if form_resources(document, form)?.is_some() {
            continue;
        }
        let Ok(content) = form_content(document, form) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        used.extend(usage.names.iter().cloned());
        extend_names_by_type(used_by_type, &usage.names_by_resource_type);
        if let Some(names) = usage.names_by_resource_type.get(b"XObject".as_slice()) {
            for (name, handle) in &xobjects {
                if names.contains(name) && is_form(document, *handle)? {
                    pending.push_back(*handle);
                }
            }
        }
    }
    Ok(used)
}

fn keep_unused_resource(selectors: &BTreeSet<String>, category: &[u8], name: &[u8]) -> bool {
    if selectors.is_empty() {
        return false;
    }
    if selectors.contains("*") {
        return true;
    }
    let category = String::from_utf8_lossy(category);
    let name = String::from_utf8_lossy(name);
    selectors.iter().any(|selector| {
        if selector == name.as_ref() {
            return true;
        }
        let Some((selector_category, selector_name)) = selector.split_once(':') else {
            return false;
        };
        selector_category == category && (selector_name == "*" || selector_name == name)
    })
}

fn resource_entry_used(
    category: &[u8],
    name: &[u8],
    used_names: &BTreeSet<Vec<u8>>,
    extra_used_names: Option<&BTreeSet<Vec<u8>>>,
    used_by_type: Option<&ResourceNamesByType>,
) -> bool {
    if matches!(category, b"Font" | b"XObject") {
        // Preserve qpdf's conservative flat-name semantics for the two legacy
        // categories: a same-named resource used by another operator is kept.
        return used_names.contains(name)
            || extra_used_names.is_some_and(|names| names.contains(name));
    }
    used_by_type
        .and_then(|usage| usage.get(category))
        .is_some_and(|names| names.contains(name))
}

fn pruned_resources(
    document: &EditDocument,
    mut resources: OwnedDictionary,
    used_names: &BTreeSet<Vec<u8>>,
    extra_used_names: Option<&BTreeSet<Vec<u8>>>,
    used_by_type: Option<&ResourceNamesByType>,
    keep_unused: &BTreeSet<String>,
) -> Result<(OwnedDictionary, ResourcePruneStats)> {
    let mut stats = ResourcePruneStats::default();
    for category in [
        b"Font".as_slice(),
        b"XObject".as_slice(),
        b"ExtGState".as_slice(),
        b"Pattern".as_slice(),
        b"Properties".as_slice(),
        b"Shading".as_slice(),
    ] {
        // Typed categories are only destructive when typed usage was actually
        // collected. This keeps legacy/shared callers without typed evidence
        // conservative rather than treating missing evidence as "unused".
        if !matches!(category, b"Font" | b"XObject") && used_by_type.is_none() {
            continue;
        }
        let Some(value) = resources.get(category).cloned() else {
            continue;
        };
        let Some(mut dictionary) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        let before = dictionary.len();
        dictionary.retain(|name, _| {
            resource_entry_used(category, name, used_names, extra_used_names, used_by_type)
                || keep_unused_resource(keep_unused, category, name)
        });
        stats.record_category(category, before.saturating_sub(dictionary.len()));
        resources.insert(category.to_vec(), OwnedObject::Dictionary(dictionary));
    }
    Ok((resources, stats))
}

fn install_page_resources(
    document: &mut EditDocument,
    page: ObjectHandle,
    resources: OwnedDictionary,
) -> Result<()> {
    let object = match page {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

fn install_form_resources(
    document: &mut EditDocument,
    form: ObjectHandle,
    resources: OwnedDictionary,
) -> Result<()> {
    let object = match form {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

pub(crate) fn should_prune_resources_hayro(document: &EditDocument) -> Result<bool> {
    let catalog = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = document.current_owned_object(catalog)? else {
        return Ok(false);
    };
    let Some(catalog) = catalog.as_dictionary() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(pages)) = catalog.get(b"Pages".as_slice()) else {
        return Ok(false);
    };

    let mut queue = VecDeque::from([*pages]);
    let mut nodes_seen = BTreeSet::new();
    let mut indirect_resources_seen = BTreeSet::new();

    while let Some(node) = queue.pop_front() {
        if !nodes_seen.insert(node) {
            continue;
        }
        let Some(object) = document.current_owned_object(node)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };

        if let Some(kids) = dictionary.get(b"Kids".as_slice())
            && let Some(OwnedObject::Array(kids)) = document.resolve_owned_value(kids)?
        {
            if dictionary.contains_key(b"Resources".as_slice()) {
                return Ok(true);
            }
            for kid in kids {
                if let OwnedObject::Reference(kid) = kid {
                    queue.push_back(kid);
                }
            }
            continue;
        }

        let Some(resources_value) = dictionary.get(b"Resources".as_slice()) else {
            continue;
        };
        if let OwnedObject::Reference(resources) = resources_value
            && !indirect_resources_seen.insert(*resources)
        {
            return Ok(true);
        }
        let Some(resources) = document.resolve_owned_value(resources_value)? else {
            continue;
        };
        let Some(resources) = resources.as_dictionary() else {
            continue;
        };
        let Some(xobjects_value) = resources.get(b"XObject".as_slice()) else {
            continue;
        };
        if let OwnedObject::Reference(xobjects) = xobjects_value
            && !indirect_resources_seen.insert(*xobjects)
        {
            return Ok(true);
        }
        let Some(xobjects) = document.resolve_owned_value(xobjects_value)? else {
            continue;
        };
        let Some(xobjects) = xobjects.as_dictionary() else {
            continue;
        };
        for value in xobjects.values() {
            let OwnedObject::Reference(handle) = value else {
                continue;
            };
            if is_form(document, *handle)? {
                queue.push_back(*handle);
            }
        }
    }

    Ok(false)
}

pub(crate) fn prune_resources_with_usage_hayro(
    document: &mut EditDocument,
    keep_unused: &BTreeSet<String>,
    page_names: &BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
    form_names: &BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
    page_names_by_type: &BTreeMap<ObjectHandle, ResourceNamesByType>,
    form_names_by_type: &BTreeMap<ObjectHandle, ResourceNamesByType>,
    generated_page_xobjects: &BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
) -> Result<ResourcePruneStats> {
    let mut stats = ResourcePruneStats::default();

    // This fast path is only called when the shared content inventory is
    // complete, which implies every reachable Form has its own resource scope.
    // Resource-less Forms require caller-scope borrowing and therefore use the
    // canonical parsing path instead.
    for (&form, names) in form_names {
        let Some(resources) = form_resources(document, form)? else {
            continue;
        };
        let (resources, pruned) = pruned_resources(
            document,
            resources,
            names,
            None,
            form_names_by_type.get(&form),
            keep_unused,
        )?;
        stats.merge(pruned);
        install_form_resources(document, form, resources)?;
    }

    for (&page, names) in page_names {
        let Some(resources) = page_resources(document, page)? else {
            continue;
        };
        let (resources, pruned) = pruned_resources(
            document,
            resources,
            names,
            generated_page_xobjects.get(&page),
            page_names_by_type.get(&page),
            keep_unused,
        )?;
        stats.merge(pruned);
        install_page_resources(document, page, resources)?;
    }
    Ok(stats)
}

pub(crate) fn prune_resources_hayro(
    document: &mut EditDocument,
    keep_unused: &BTreeSet<String>,
) -> Result<ResourcePruneStats> {
    let mut stats = ResourcePruneStats::default();
    // Prune forms with local resource scopes first. Resource-less forms are
    // intentionally accounted against their caller's scope below.
    let mut forms = BTreeSet::new();
    for handle in document.reachable_output_objects()? {
        if is_form(document, handle)? && form_resources(document, handle)?.is_some() {
            forms.insert(handle);
        }
    }
    for form in forms {
        let Some(resources) = form_resources(document, form)? else {
            continue;
        };
        let Ok(content) = form_content(document, form) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        let mut names = usage.names.clone();
        let mut names_by_type = usage.names_by_resource_type.clone();
        names.extend(borrowed_form_names(
            document,
            &resources,
            &usage,
            &mut names_by_type,
        )?);
        let (resources, pruned) = pruned_resources(
            document,
            resources,
            &names,
            None,
            Some(&names_by_type),
            keep_unused,
        )?;
        stats.merge(pruned);
        install_form_resources(document, form, resources)?;
    }

    for page in document.page_handles()? {
        let Some(resources) = page_resources(document, page)? else {
            continue;
        };
        let Ok(content) = page_content(document, page) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        let mut names = usage.names.clone();
        let mut names_by_type = usage.names_by_resource_type.clone();
        names.extend(borrowed_form_names(
            document,
            &resources,
            &usage,
            &mut names_by_type,
        )?);
        let (resources, pruned) = pruned_resources(
            document,
            resources,
            &names,
            None,
            Some(&names_by_type),
            keep_unused,
        )?;
        stats.merge(pruned);
        install_page_resources(document, page, resources)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod resource_retention_tests {
    use super::{ResourceNamesByType, keep_unused_resource, resource_entry_used};
    use std::collections::BTreeSet;

    fn selectors(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn unused_resource_selectors_match_expected_scope() {
        assert!(keep_unused_resource(&selectors(&["*"]), b"Font", b"F1"));
        assert!(keep_unused_resource(
            &selectors(&["XObject:*"]),
            b"XObject",
            b"Im7"
        ));
        assert!(!keep_unused_resource(
            &selectors(&["XObject:*"]),
            b"Font",
            b"F1"
        ));
        assert!(keep_unused_resource(
            &selectors(&["Font:F1"]),
            b"Font",
            b"F1"
        ));
        assert!(!keep_unused_resource(
            &selectors(&["Font:F1"]),
            b"Font",
            b"F2"
        ));
        assert!(keep_unused_resource(
            &selectors(&["SharedName"]),
            b"XObject",
            b"SharedName"
        ));
    }

    #[test]
    fn typed_resource_usage_does_not_cross_namespaces() {
        let flat = BTreeSet::from([b"Shared".to_vec()]);
        let typed = ResourceNamesByType::from([
            (b"ExtGState".to_vec(), BTreeSet::from([b"Shared".to_vec()])),
            (b"Pattern".to_vec(), BTreeSet::from([b"P0".to_vec()])),
        ]);

        // Preserve legacy qpdf-style flat-name semantics for Font/XObject.
        assert!(resource_entry_used(
            b"Font",
            b"Shared",
            &flat,
            None,
            Some(&typed)
        ));
        assert!(resource_entry_used(
            b"XObject",
            b"Shared",
            &flat,
            None,
            Some(&typed)
        ));

        assert!(resource_entry_used(
            b"ExtGState",
            b"Shared",
            &flat,
            None,
            Some(&typed)
        ));
        assert!(!resource_entry_used(
            b"Pattern",
            b"Shared",
            &flat,
            None,
            Some(&typed)
        ));
        assert!(resource_entry_used(
            b"Pattern",
            b"P0",
            &flat,
            None,
            Some(&typed)
        ));
        assert!(!resource_entry_used(
            b"Shading",
            b"P0",
            &flat,
            None,
            Some(&typed)
        ));
    }

    #[test]
    fn generated_page_xobjects_extend_flat_usage_without_copying_maps() {
        let flat = BTreeSet::new();
        let generated = BTreeSet::from([b"Generated".to_vec()]);

        assert!(resource_entry_used(
            b"XObject",
            b"Generated",
            &flat,
            Some(&generated),
            None
        ));
        // Match the legacy flat-name behavior used by the old cloned overlay.
        assert!(resource_entry_used(
            b"Font",
            b"Generated",
            &flat,
            Some(&generated),
            None
        ));
        assert!(!resource_entry_used(
            b"Pattern",
            b"Generated",
            &flat,
            Some(&generated),
            None
        ));
    }
}
