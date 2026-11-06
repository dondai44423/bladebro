// Upload readback (action/upload.rs), called on the file input right after
// DOM.setFileInputFiles. Any path string yields a File; only a read proves
// the browser can open it: a path outside a sandbox's view gives an empty
// File (size 0) whose read fails with NotFoundError. One byte is enough for
// a non-empty file; an empty one is read whole.
async function uploadReadback() {
    var files = this.files;
    var f = files && files[0];
    if (!f) return {count: files ? files.length : 0};
    var read = 'ok';
    try {
        await (f.size > 0 ? f.slice(0, 1) : f).arrayBuffer();
    } catch (e) {
        read = (e && e.name) || 'error';
    }
    return {count: files.length, name: f.name, size: f.size, read: read};
}
