// This runs in find_by_sig's resolved frame, without focusing or mutating it.
if (mode === 'inspect') {
    var control = lp(n) || n;
    var data = {tag: control.tagName.toLowerCase()};
    if (control.tagName === 'INPUT' || control.tagName === 'TEXTAREA' || control.tagName === 'SELECT') {
        data.value = control.value;
    } else {
        data.text = n.innerText || n.textContent || '';
    }
    if (control.tagName === 'INPUT') data.type = control.type;
    if (control.type === 'checkbox' || control.type === 'radio') {
        data.checked = control.checked;
        if (control.type === 'checkbox') data.indeterminate = control.indeterminate;
    }
    if (control.tagName === 'SELECT') {
        if (control.multiple) data.values = Array.from(control.selectedOptions, o => o.value);
        data.options = Array.from({length: Math.min(control.options.length, 80)}, (_, i) => {
            var o = control.options[i];
            return {label: o.label, value: o.value, selected: o.selected,
                disabled: o.disabled || (o.parentElement.tagName === 'OPTGROUP' && o.parentElement.disabled)};
        });
        data.options_total = control.options.length;
        if (data.options_total > 80) data.options_truncated = true;
    }
    if (control.disabled || control.matches(':disabled')) data.disabled = true;
    if (control.readOnly) data.readonly = true;
    if (control.required) data.required = true;
    for (var attr of ['aria-checked', 'aria-selected', 'aria-pressed', 'aria-expanded', 'aria-invalid']) {
        if (control.hasAttribute(attr)) data[attr] = control.getAttribute(attr);
    }
    return {ok: true, inspection: data};
}
