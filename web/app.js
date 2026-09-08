import init, { analyze, optimize_with_hidden_text } from './pkg/pdf_deshit_wasm.js';

await init();

const fileInput = document.querySelector('#file');
const drop = document.querySelector('#drop');
const go = document.querySelector('#go');
const status = document.querySelector('#status');
const result = document.querySelector('#result');
const hiddenReview = document.querySelector('#hidden-review');
const hiddenSummary = document.querySelector('#hidden-summary');
const hiddenGroups = document.querySelector('#hidden-groups');

let selected = null;
let selectedBytes = null;
let analysis = null;
let downloadUrl = null;

const categoryOrder = [
  'likely-redaction-leak',
  'ocr-overlay',
  'accessibility',
  'hidden-layer',
  'outside-page',
  'other-invisible',
];

const categoryInfo = {
  'likely-redaction-leak': {
    title: 'Probable redaction leaks',
    description: 'Text covered by a small dark opaque region. Removal is recommended when confidence is high.',
  },
  'ocr-overlay': {
    title: 'OCR text',
    description: 'Invisible text associated with rasterized pages. Usually useful for search and accessibility.',
  },
  accessibility: {
    title: 'Accessibility text',
    description: 'ActualText or equivalent semantic text. Kept by default.',
  },
  'hidden-layer': {
    title: 'Hidden layers',
    description: 'Text in optional content that is off in the default view.',
  },
  'outside-page': {
    title: 'Outside the visible page',
    description: 'Text positioned outside the CropBox.',
  },
  'other-invisible': {
    title: 'Other invisible text',
    description: 'Invisible text without a strong semantic classification. Kept by default.',
  },
};

const size = n => {
  const units = ['B', 'KiB', 'MiB', 'GiB'];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${n.toFixed(i ? 2 : 0)} ${units[i]}`;
};

function clearReview() {
  analysis = null;
  hiddenReview.classList.add('hidden');
  hiddenGroups.replaceChildren();
  hiddenSummary.textContent = '';
  go.textContent = 'Deshittify';
}

function choose(file) {
  if (!file) return;
  selected = file;
  selectedBytes = null;
  clearReview();
  drop.querySelector('strong').textContent = file.name;
  drop.querySelector('span').textContent = size(file.size);
  go.disabled = false;
  result.classList.add('hidden');
  status.textContent = '';
}

fileInput.addEventListener('change', () => choose(fileInput.files[0]));
for (const ev of ['dragenter', 'dragover']) {
  drop.addEventListener(ev, e => {
    e.preventDefault();
    drop.classList.add('drag');
  });
}
for (const ev of ['dragleave', 'drop']) {
  drop.addEventListener(ev, e => {
    e.preventDefault();
    drop.classList.remove('drag');
  });
}
drop.addEventListener('drop', e => choose(e.dataTransfer.files[0]));

async function bytesForSelected() {
  if (!selectedBytes) {
    selectedBytes = new Uint8Array(await selected.arrayBuffer());
  }
  return selectedBytes;
}

function confidenceText(value) {
  if (!Number.isFinite(value)) return '';
  return `${Math.round(value * 100)}% confidence`;
}

function findingLabel(finding) {
  const text = finding.text?.trim();
  if (text) return text;
  if (finding.raw_hex) return `raw bytes: ${finding.raw_hex}`;
  return '(text could not be decoded)';
}

function updateGroupState(group) {
  const groupToggle = group.querySelector('.group-remove');
  const items = [...group.querySelectorAll('.finding-remove')];
  const checked = items.filter(item => item.checked).length;
  groupToggle.checked = checked === items.length && items.length > 0;
  groupToggle.indeterminate = checked > 0 && checked < items.length;
}

function renderHiddenReview(findings) {
  hiddenGroups.replaceChildren();
  hiddenSummary.textContent = `${findings.length} invisible text item${findings.length === 1 ? '' : 's'} found. Review the defaults below, then run the optimizer again.`;

  const byCategory = new Map();
  for (const finding of findings) {
    const list = byCategory.get(finding.category) ?? [];
    list.push(finding);
    byCategory.set(finding.category, list);
  }

  const categories = [
    ...categoryOrder.filter(category => byCategory.has(category)),
    ...[...byCategory.keys()].filter(category => !categoryOrder.includes(category)),
  ];

  for (const category of categories) {
    const findingsInCategory = byCategory.get(category);
    const info = categoryInfo[category] ?? {
      title: category,
      description: 'Unrecognized category from this version of the analyzer.',
    };

    const group = document.createElement('details');
    group.className = 'hidden-group';
    group.dataset.category = category;
    group.open = category === 'likely-redaction-leak';

    const summary = document.createElement('summary');
    summary.textContent = `${info.title} (${findingsInCategory.length})`;
    group.append(summary);

    const description = document.createElement('p');
    description.className = 'hint';
    description.textContent = info.description;
    group.append(description);

    const groupLabel = document.createElement('label');
    groupLabel.className = 'check-row group-control';
    const groupToggle = document.createElement('input');
    groupToggle.type = 'checkbox';
    groupToggle.className = 'group-remove';
    const groupText = document.createElement('span');
    groupText.textContent = 'Remove this category';
    groupLabel.append(groupToggle, groupText);
    group.append(groupLabel);

    const list = document.createElement('div');
    list.className = 'finding-list';
    for (const finding of findingsInCategory) {
      const item = document.createElement('details');
      item.className = 'finding';

      const itemSummary = document.createElement('summary');
      const toggle = document.createElement('input');
      toggle.type = 'checkbox';
      toggle.className = 'finding-remove';
      toggle.checked = finding.suggested_action === 'remove';
      toggle.dataset.findingId = finding.id;
      toggle.addEventListener('click', event => event.stopPropagation());
      toggle.addEventListener('change', () => updateGroupState(group));

      const label = document.createElement('span');
      label.className = 'finding-label';
      label.textContent = findingLabel(finding);
      itemSummary.append(toggle, label);
      item.append(itemSummary);

      const meta = document.createElement('div');
      meta.className = 'finding-meta';
      const parts = [
        `page ${finding.page_number}`,
        finding.mechanism,
        confidenceText(finding.confidence),
      ].filter(Boolean);
      if (finding.artifact) parts.push('marked /Artifact');
      meta.textContent = parts.join(' · ');
      item.append(meta);

      if (finding.bounds) {
        const bounds = document.createElement('div');
        bounds.className = 'finding-meta';
        const b = finding.bounds;
        bounds.textContent = `bounds: ${b.x0.toFixed(1)}, ${b.y0.toFixed(1)} → ${b.x1.toFixed(1)}, ${b.y1.toFixed(1)}`;
        item.append(bounds);
      }

      list.append(item);
    }
    group.append(list);

    groupToggle.addEventListener('change', () => {
      for (const item of group.querySelectorAll('.finding-remove')) {
        item.checked = groupToggle.checked;
      }
      updateGroupState(group);
    });
    updateGroupState(group);
    hiddenGroups.append(group);
  }

  hiddenReview.classList.remove('hidden');
}

function hiddenTextPolicy() {
  const removeCategories = [];
  const overrides = {};

  for (const group of hiddenGroups.querySelectorAll('.hidden-group')) {
    const category = group.dataset.category;
    const items = [...group.querySelectorAll('.finding-remove')];
    const checked = items.filter(item => item.checked);

    // Choose the smaller representation: category default plus keep overrides,
    // or the default keep policy plus remove overrides.
    if (checked.length > items.length / 2) {
      removeCategories.push(category);
      for (const item of items) {
        if (!item.checked) overrides[item.dataset.findingId] = 'keep';
      }
    } else {
      for (const item of checked) {
        overrides[item.dataset.findingId] = 'remove';
      }
    }
  }

  return {
    remove_categories: removeCategories,
    overrides,
  };
}

async function optimizeSelected(bytes) {
  status.textContent = 'Working locally…';
  await new Promise(requestAnimationFrame);

  const policy = analysis ? hiddenTextPolicy() : { remove_categories: [], overrides: {} };
  const r = optimize_with_hidden_text(
    bytes,
    document.querySelector('#profile').value,
    document.querySelector('#privacy').value,
    policy,
  );
  const output = new Uint8Array(r.pdf);
  if (downloadUrl) URL.revokeObjectURL(downloadUrl);
  downloadUrl = URL.createObjectURL(new Blob([output], { type: 'application/pdf' }));

  const a = document.querySelector('#download');
  a.href = downloadUrl;
  a.download = selected.name.replace(/\.pdf$/i, '') + '.deshit.pdf';
  document.querySelector('#before').textContent = size(r.report.before.input_bytes);
  document.querySelector('#after').textContent = size(r.report.after_bytes);
  document.querySelector('#saved').textContent = `${r.report.saved_percent.toFixed(1)}%`;
  document.querySelector('#report').textContent = JSON.stringify(r.report, null, 2);
  result.classList.remove('hidden');
  status.textContent = r.report.hidden_text_items_removed
    ? `Done. Removed ${r.report.hidden_text_items_removed} approved invisible text item${r.report.hidden_text_items_removed === 1 ? '' : 's'}.`
    : 'Done.';
}

go.addEventListener('click', async () => {
  if (!selected) return;
  go.disabled = true;
  try {
    const bytes = await bytesForSelected();
    if (!analysis) {
      status.textContent = 'Analyzing locally…';
      await new Promise(requestAnimationFrame);
      analysis = analyze(bytes);
      const findings = analysis.hidden_text ?? [];
      if (findings.length > 0) {
        renderHiddenReview(findings);
        go.textContent = 'Deshittify with these choices';
        status.textContent = 'Review the invisible-text findings before rewriting the PDF.';
        return;
      }
    }
    await optimizeSelected(bytes);
  } catch (e) {
    console.error(e);
    status.textContent = `Failed: ${e}`;
  } finally {
    go.disabled = false;
  }
});
