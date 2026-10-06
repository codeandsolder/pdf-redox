use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    content::{form_content, form_resources, page_content, page_resources, resolved_dictionary},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

type ResourceNamesByType = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

#[derive(Debug, Clone, Default)]
struct DetachedResourceUsage {
    names_by_resource_type: ResourceNamesByType,
}

const fn operator_resource_type(operator: &[u8]) -> Option<&'static [u8]> {
    match operator {
        b"CS" | b"cs" => Some(b"ColorSpace"),
        b"gs" => Some(b"ExtGState"),
        b"Tf" => Some(b"Font"),
        b"SCN" | b"scn" => Some(b"Pattern"),
        b"BDC" | b"DP" => Some(b"Properties"),
        b"sh" => Some(b"Shading"),
        b"Do" => Some(b"XObject"),
        _ => None,
    }
}

fn scan(content: &[u8]) -> Option<DetachedResourceUsage> {
    let mut usage = DetachedResourceUsage::default();
    let incomplete = crate::content_stream::visit_instructions(content, |instruction| {
        let Some(resource_type) = operator_resource_type(&instruction.operator[..]) else {
            return Ok(());
        };
        let mut last_name = None;
        for operand in instruction.operands() {
            if let Some(name) = crate::content_stream::operand_name(operand) {
                last_name = Some(name);
            }
        }
        if let Some(name) = last_name {
            let name = name.to_vec();
            usage
                .names_by_resource_type
                .entry(resource_type.to_vec())
                .or_default()
                .insert(name);
        }
        Ok(())
    })
    .ok()?;
    (!incomplete).then_some(usage)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_field_names,
    reason = "the repeated entries_removed suffix makes each public resource-pruning counter self-describing"
)]
pub struct ResourcePruneStats {
    pub entries_removed: usize,
    pub font_entries_removed: usize,
    pub xobject_entries_removed: usize,
    pub ext_gstate_entries_removed: usize,
    pub pattern_entries_removed: usize,
    pub properties_entries_removed: usize,
    pub shading_entries_removed: usize,
}

impl ResourcePruneStats {
    const fn record_category(&mut self, category: &[u8], count: usize) {
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

    const fn merge(&mut self, other: Self) {
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

fn extend_names_by_type(target: &mut ResourceNamesByType, source: &ResourceNamesByType) {
    for (resource_type, names) in source {
        target
            .entry(resource_type.clone())
            .or_default()
            .extend(names.iter().cloned());
    }
}

fn extend_borrowed_form_usage(
    document: &EditDocument,
    resources: &OwnedDictionary,
    root_usage: &DetachedResourceUsage,
    used_by_type: &mut ResourceNamesByType,
) -> Result<()> {
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
        extend_names_by_type(used_by_type, &usage.names_by_resource_type);
        if let Some(names) = usage.names_by_resource_type.get(b"XObject".as_slice()) {
            for (name, handle) in &xobjects {
                if names.contains(name) && is_form(document, *handle)? {
                    pending.push_back(*handle);
                }
            }
        }
    }
    Ok(())
}

fn resource_entry_used(
    category: &[u8],
    name: &[u8],
    used_by_type: &ResourceNamesByType,
    extra_xobjects: Option<&BTreeSet<Vec<u8>>>,
) -> bool {
    used_by_type
        .get(category)
        .is_some_and(|names| names.contains(name))
        || (category == b"XObject" && extra_xobjects.is_some_and(|names| names.contains(name)))
}

fn pruned_resources(
    document: &EditDocument,
    mut resources: OwnedDictionary,
    used_by_type: &ResourceNamesByType,
    extra_xobjects: Option<&BTreeSet<Vec<u8>>>,
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
        let Some(value) = resources.get(category).cloned() else {
            continue;
        };
        let Some(mut dictionary) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        let before = dictionary.len();
        dictionary
            .retain(|name, _| resource_entry_used(category, name, used_by_type, extra_xobjects));
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
    let object = document.edit_handle(page)?;
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
    let object = document.edit_handle(form)?;
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

pub fn should_prune_resources(document: &EditDocument) -> Result<bool> {
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

pub fn prune_xobject_candidates_for_content(
    document: &EditDocument,
    mut resources: OwnedDictionary,
    content: &[u8],
    candidates: &BTreeSet<Vec<u8>>,
) -> Result<(OwnedDictionary, usize)> {
    if candidates.is_empty() {
        return Ok((resources, 0));
    }
    let Some(usage) = scan(content) else {
        return Ok((resources, 0));
    };
    let mut names_by_type = usage.names_by_resource_type.clone();
    extend_borrowed_form_usage(document, &resources, &usage, &mut names_by_type)?;

    let Some(value) = resources.get(b"XObject".as_slice()).cloned() else {
        return Ok((resources, 0));
    };
    let Some(mut xobjects) = resolved_dictionary(document, Some(&value))? else {
        return Ok((resources, 0));
    };
    let before = xobjects.len();
    xobjects.retain(|name, _| {
        !candidates.contains(name) || resource_entry_used(b"XObject", name, &names_by_type, None)
    });
    let removed = before.saturating_sub(xobjects.len());
    if removed != 0 {
        resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
    }
    Ok((resources, removed))
}

pub fn prune_resources_with_usage(
    document: &mut EditDocument,
    page_names_by_type: &BTreeMap<ObjectHandle, ResourceNamesByType>,
    form_names_by_type: &BTreeMap<ObjectHandle, ResourceNamesByType>,
    generated_page_xobjects: &BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
) -> Result<ResourcePruneStats> {
    let mut stats = ResourcePruneStats::default();

    // This fast path is only called when the shared content inventory is
    // complete, which implies every reachable Form has its own resource scope.
    // Resource-less Forms require caller-scope borrowing and therefore use the
    // canonical parsing path instead.
    for (&form, names_by_type) in form_names_by_type {
        let Some(resources) = form_resources(document, form)? else {
            continue;
        };
        let (resources, pruned) = pruned_resources(document, resources, names_by_type, None)?;
        stats.merge(pruned);
        install_form_resources(document, form, resources)?;
    }

    for (&page, names_by_type) in page_names_by_type {
        let Some(resources) = page_resources(document, page)? else {
            continue;
        };
        let (resources, pruned) = pruned_resources(
            document,
            resources,
            names_by_type,
            generated_page_xobjects.get(&page),
        )?;
        stats.merge(pruned);
        install_page_resources(document, page, resources)?;
    }
    Ok(stats)
}

pub fn prune_resources(document: &mut EditDocument) -> Result<ResourcePruneStats> {
    let mut stats = ResourcePruneStats::default();
    // Prune forms with local resource scopes first. Resource-less forms are
    // intentionally accounted against their caller's scope below.
    let mut forms = BTreeSet::new();
    for handle in document.reachable_streams_with_subtype(b"Form")? {
        if form_resources(document, handle)?.is_some() {
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
        let mut names_by_type = usage.names_by_resource_type.clone();
        extend_borrowed_form_usage(document, &resources, &usage, &mut names_by_type)?;
        let (resources, pruned) = pruned_resources(document, resources, &names_by_type, None)?;
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
        let mut names_by_type = usage.names_by_resource_type.clone();
        extend_borrowed_form_usage(document, &resources, &usage, &mut names_by_type)?;
        let (resources, pruned) = pruned_resources(document, resources, &names_by_type, None)?;
        stats.merge(pruned);
        install_page_resources(document, page, resources)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod resource_retention_tests {
    use super::{ResourceNamesByType, resource_entry_used};
    use std::collections::BTreeSet;

    #[test]
    fn typed_resource_usage_does_not_cross_namespaces() {
        let typed = ResourceNamesByType::from([
            (b"ExtGState".to_vec(), BTreeSet::from([b"Shared".to_vec()])),
            (b"Pattern".to_vec(), BTreeSet::from([b"P0".to_vec()])),
        ]);

        assert!(!resource_entry_used(b"Font", b"Shared", &typed, None));
        assert!(!resource_entry_used(b"XObject", b"Shared", &typed, None));
        assert!(resource_entry_used(b"ExtGState", b"Shared", &typed, None));
        assert!(!resource_entry_used(b"Pattern", b"Shared", &typed, None));
        assert!(resource_entry_used(b"Pattern", b"P0", &typed, None));
        assert!(!resource_entry_used(b"Shading", b"P0", &typed, None));
    }

    #[test]
    fn generated_page_xobjects_are_scoped_to_xobject_namespace() {
        let typed = ResourceNamesByType::new();
        let generated = BTreeSet::from([b"Generated".to_vec()]);

        assert!(resource_entry_used(
            b"XObject",
            b"Generated",
            &typed,
            Some(&generated)
        ));
        assert!(!resource_entry_used(
            b"Font",
            b"Generated",
            &typed,
            Some(&generated)
        ));
        assert!(!resource_entry_used(
            b"Pattern",
            b"Generated",
            &typed,
            Some(&generated)
        ));
    }
}
