//! Loads the AR SDK's bundled libraries from its root by absolute path, so
//! the process needs no `LD_LIBRARY_PATH`.
//!
//! The same approach as the Video Effects backend (`obc-backend-maxine`'s
//! `preload`), copied here so that this module depends on nothing of that
//! SDK's. The loader reads `LD_LIBRARY_PATH` only when the process starts,
//! so a running process cannot add the SDK's directories to its search
//! path. Instead, before `libNVCVImage.so` and `libnvARPose.so` are opened,
//! every library they and the SDK's own later `dlopen` calls need is opened
//! here with `RTLD_GLOBAL`, dependencies first, by its absolute path in the
//! root. The later lookups by name (a `DT_NEEDED` entry, the SDK opening
//! `libnvARPoseLocal.so` and the feature libraries) then find the objects
//! already loaded: the loader matches a requested name against the
//! `DT_SONAME` of the objects it has loaded.
//!
//! Only the libraries the module needs are loaded: the roots ([`roots`]:
//! the core libraries and those of the features in [`crate::FEATURES`])
//! and their `DT_NEEDED` closure, each name resolved in the SDK's
//! directories ([`search_dirs`]). A name found in none of them (`libc.so.6`,
//! the driver's `libcuda.so.1`) is left to the system loader. TensorRT's
//! `libnvinfer.so.10` opens the bundled `libnvJitLink.so.12` by name at run
//! time, so that one is loaded here too.
//!
//! The handles are never closed: the SDK stays mapped for the life of the
//! process, as [`crate::ffi`] keeps it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// The bundled CUDA library TensorRT (`libnvinfer.so.10`) opens by name at
/// run time; no library lists it as a dependency.
const NVJITLINK: &str = "external/cuda/lib/libnvJitLink.so.12";

/// What [`preload`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Preloaded {
    /// The libraries opened, in load order.
    pub loaded: Vec<PathBuf>,
    /// The libraries that could not be opened, with the loader's error.
    pub failed: Vec<(PathBuf, String)>,
}

impl Preloaded {
    /// The failures as one sentence fragment, or `None` when there are none.
    pub(crate) fn failures(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let list: Vec<String> = self
            .failed
            .iter()
            .map(|(p, e)| format!("{}: {e}", p.display()))
            .collect();
        Some(list.join("; "))
    }
}

/// Opens the SDK libraries of `root` that the core libraries `core` (file
/// names in `<root>/lib`) and the features `features` (directory names in
/// `<root>/features`) need, dependencies first. A library that cannot be
/// opened is recorded and the rest are still tried: what the SDK then fails
/// to find is reported when it is opened.
pub(crate) fn preload(root: &Path, core: &[&str], features: &[&str]) -> Preloaded {
    let mut done = Preloaded::default();
    for path in plan(root, core, features) {
        match dlopen_global(&path) {
            Ok(()) => done.loaded.push(path),
            Err(e) => done.failed.push((path, e)),
        }
    }
    done
}

/// Opens `path` with `RTLD_LAZY | RTLD_GLOBAL` and leaks the handle.
fn dlopen_global(path: &Path) -> Result<(), String> {
    let c =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| "a NUL in the path".to_string())?;
    // SAFETY: `c` is a NUL-terminated path. Loading an SDK library runs its
    // initialisers, which have no preconditions for the caller.
    let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        // SAFETY: dlerror returns null or a NUL-terminated string owned by
        // the loader, valid until the next dl call on this thread.
        let e = unsafe { libc::dlerror() };
        let msg = if e.is_null() {
            "unknown loader error".to_string()
        } else {
            // SAFETY: see above.
            unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
        };
        return Err(msg);
    }
    // The handle is deliberately leaked: the library stays loaded.
    Ok(())
}

/// The libraries to open, in load order: the closure of [`roots`] over
/// `DT_NEEDED` within [`search_dirs`], dependencies first, each file once
/// (by canonical path).
pub(crate) fn plan(root: &Path, core: &[&str], features: &[&str]) -> Vec<PathBuf> {
    let dirs = search_dirs(root);
    let start: Vec<PathBuf> = roots(root, core, features)
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    let mut deps: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut todo = start.clone();
    while let Some(lib) = todo.pop() {
        if deps.contains_key(&lib) {
            continue;
        }
        let found: Vec<PathBuf> = needed_of_file(&lib)
            .iter()
            .filter_map(|name| resolve(name, &dirs))
            .collect();
        todo.extend(found.iter().filter(|d| !deps.contains_key(*d)).cloned());
        deps.insert(lib, found);
    }
    load_order(&start, &deps)
}

/// The SDK directories a needed name is looked up in, in order: `lib`,
/// each `features/*/lib` (sorted), the bundled CUDA and TensorRT.
pub(crate) fn search_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.join("lib")];
    dirs.extend(feature_lib_dirs(root));
    dirs.push(root.join("external/cuda/lib"));
    dirs.push(root.join("external/tensorrt/lib"));
    dirs
}

/// The libraries the closure starts from: each of `core` from `<root>/lib`
/// (under its plain name or its major-versioned soname, whichever is
/// there), `libnvJitLink.so.12` from
/// `<root>/external/cuda/lib` when it is there ([`NVJITLINK`]), and every
/// shared object in `<root>/features/<feature>/lib` of `features`, which the
/// SDK opens when a feature is created. The Triton client library is not
/// one: it serves the SDK's remote-inference mode, which this module does
/// not use.
pub(crate) fn roots(root: &Path, core: &[&str], features: &[&str]) -> Vec<PathBuf> {
    let lib = root.join("lib");
    let mut out: Vec<PathBuf> = core
        .iter()
        .filter_map(|name| {
            [lib.join(name), lib.join(format!("{name}.1"))]
                .into_iter()
                .find(|p| p.is_file())
        })
        .collect();
    out.extend(Some(root.join(NVJITLINK)).filter(|p| p.is_file()));
    for f in features {
        let dir = root.join("features").join(f).join("lib");
        out.extend(shared_objects(&dir));
    }
    out
}

/// The shared objects in `dir`, sorted (none when it cannot be read).
pub(crate) fn shared_objects(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut libs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_shared_object_name(p))
        .collect();
    libs.sort();
    libs
}

/// `<root>/features/*/lib`, sorted.
fn feature_lib_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("features")) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path().join("lib"))
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// `libx.so` or `libx.so.<version>`.
fn is_shared_object_name(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".so") || n.contains(".so."))
}

/// `name` (a `DT_NEEDED` entry) in the first of `dirs` that has it, by
/// canonical path; `None` (a system library) when none has it. A name with
/// a slash is a path for the system loader, never looked up here.
pub(crate) fn resolve(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    dirs.iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
}

/// The order to load `roots` and their dependencies in: every node after
/// the nodes it depends on (`deps`), roots in the order given and
/// dependencies in their listed order otherwise, each node once. A cycle is
/// broken where it closes: a node already on the path is not visited again.
/// Nodes reached only through `deps` are included; a node missing from
/// `deps` has no dependencies.
pub(crate) fn load_order<K: Ord + Clone>(roots: &[K], deps: &BTreeMap<K, Vec<K>>) -> Vec<K> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<K> = BTreeSet::new();
    for r in roots {
        if !seen.insert(r.clone()) {
            continue;
        }
        // Depth first, iteratively: (node, index of its next dependency).
        let mut stack: Vec<(K, usize)> = vec![(r.clone(), 0)];
        while let Some((node, i)) = stack.last_mut() {
            let next = deps.get(node).and_then(|list| list.get(*i)).cloned();
            *i += 1;
            match next {
                Some(next) => {
                    if seen.insert(next.clone()) {
                        stack.push((next, 0));
                    }
                }
                None => {
                    if let Some((done, _)) = stack.pop() {
                        out.push(done);
                    }
                }
            }
        }
    }
    out
}

/// The `DT_NEEDED` names of the ELF file at `path`; empty when it cannot
/// be read or is not a 64-bit little-endian ELF object.
pub(crate) fn needed_of_file(path: &Path) -> Vec<String> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    needed(|offset, len| {
        let mut buf = vec![0u8; len];
        file.read_exact_at(&mut buf, offset).ok().map(|()| buf)
    })
}

/// The `DT_NEEDED` names of the ELF image in `bytes` (see [`needed`]).
#[cfg(test)]
pub(crate) fn needed_of_bytes(bytes: &[u8]) -> Vec<String> {
    needed(|offset, len| {
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(len)?;
        bytes.get(start..end).map(<[u8]>::to_vec)
    })
}

// ELF64 constants (System V gABI).
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
/// `e_phnum` meaning "the count is elsewhere" (`PN_XNUM`): not supported.
const PN_XNUM: u16 = 0xffff;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DYN_SIZE: usize = 16;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_STRSZ: u64 = 10;
/// The largest program header entry accepted (real ones are
/// [`PHDR_SIZE`]), which bounds the program header table read.
const MAX_PHENTSIZE: usize = 256;
/// The largest dynamic section read (far above any real one).
const MAX_DYNAMIC: u64 = 1 << 20;
/// The longest needed name read.
const MAX_NAME: u64 = 4096;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A `PT_LOAD` segment's file mapping.
#[derive(Clone, Copy)]
struct Load {
    vaddr: u64,
    offset: u64,
    filesz: u64,
}

impl Load {
    /// The file offset of virtual address `addr` and how many file bytes
    /// of the segment follow it, if the segment maps it from the file.
    fn file_offset(&self, addr: u64) -> Option<(u64, u64)> {
        let rel = addr.checked_sub(self.vaddr)?;
        if rel >= self.filesz {
            return None;
        }
        Some((self.offset.checked_add(rel)?, self.filesz - rel))
    }
}

/// The `DT_NEEDED` names of an ELF64 little-endian image read through
/// `read(offset, len)` (`None`: those bytes are not there): ELF header,
/// program headers, `PT_DYNAMIC`, then each name in the string table
/// `DT_STRTAB` points at, mapped to a file offset through the `PT_LOAD`
/// segment that contains it. Anything malformed or of another class or
/// byte order is "no names"; a single unreadable name is skipped. Never
/// panics.
fn needed(mut read: impl FnMut(u64, usize) -> Option<Vec<u8>>) -> Vec<String> {
    needed_inner(&mut read).unwrap_or_default()
}

fn needed_inner(read: &mut impl FnMut(u64, usize) -> Option<Vec<u8>>) -> Option<Vec<String>> {
    let eh = read(0, EHDR_SIZE)?;
    if eh.get(0..4)? != ELF_MAGIC || *eh.get(4)? != ELFCLASS64 || *eh.get(5)? != ELFDATA2LSB {
        return None;
    }
    let phoff = u64_at(&eh, 0x20)?;
    let phentsize = usize::from(u16_at(&eh, 0x36)?);
    let phnum = u16_at(&eh, 0x38)?;
    if phnum == PN_XNUM || !(PHDR_SIZE..=MAX_PHENTSIZE).contains(&phentsize) {
        return None;
    }
    let table = read(phoff, phentsize.checked_mul(usize::from(phnum))?)?;
    let mut loads = Vec::new();
    let mut dynamic = None;
    for i in 0..usize::from(phnum) {
        let ph = table.get(i * phentsize..i * phentsize + PHDR_SIZE)?;
        let (offset, vaddr, filesz) = (u64_at(ph, 8)?, u64_at(ph, 16)?, u64_at(ph, 32)?);
        match u32_at(ph, 0)? {
            PT_LOAD => loads.push(Load {
                vaddr,
                offset,
                filesz,
            }),
            PT_DYNAMIC => dynamic = Some((offset, filesz)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic?;
    let dyn_size = usize::try_from(dyn_size.min(MAX_DYNAMIC)).ok()?;
    let dyns = read(dyn_off, dyn_size - dyn_size % DYN_SIZE)?;
    let mut names = Vec::new();
    let (mut strtab, mut strsz) = (None, None);
    for entry in dyns.as_chunks::<DYN_SIZE>().0 {
        let (tag, val) = (u64_at(entry, 0)?, u64_at(entry, 8)?);
        match tag {
            DT_NULL => break,
            DT_NEEDED => names.push(val),
            DT_STRTAB => strtab = Some(val),
            DT_STRSZ => strsz = Some(val),
            _ => {}
        }
    }
    if names.is_empty() {
        return Some(Vec::new());
    }
    let strtab = strtab?;
    let (str_off, seg_left) = loads.iter().find_map(|l| l.file_offset(strtab))?;
    let limit = strsz.map_or(seg_left, |s| s.min(seg_left));
    let mut out = Vec::new();
    for name_off in names {
        if name_off >= limit {
            continue;
        }
        let len = (limit - name_off).min(MAX_NAME);
        let Some(bytes) = str_off
            .checked_add(name_off)
            .and_then(|at| read(at, usize::try_from(len).ok()?))
        else {
            continue;
        };
        let Some(end) = bytes.iter().position(|&b| b == 0) else {
            continue;
        };
        if let Ok(s) = std::str::from_utf8(&bytes[..end])
            && !s.is_empty()
        {
            out.push(s.to_string());
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE: &[&str] = &["libNVCVImage.so", "libnvARPoseLocal.so", "libnvARPose.so"];
    const FX: &[&str] = &["fx"];

    fn map(edges: &[(&'static str, &[&'static str])]) -> BTreeMap<&'static str, Vec<&'static str>> {
        edges.iter().map(|(k, v)| (*k, v.to_vec())).collect()
    }

    #[test]
    fn dependencies_load_first_each_once_in_a_fixed_order() {
        let deps = map(&[
            ("nvcv", &["cudart", "npp"]),
            ("npp", &["cudart"]),
            ("local", &["cudart", "nvcv", "cudnn"]),
            ("pose", &["nvcv", "cudart"]),
            ("feature", &["local", "nvcv", "trt"]),
            ("trt", &[]),
        ]);
        let order = load_order(&["nvcv", "local", "pose", "feature"], &deps);
        assert_eq!(
            order,
            [
                "cudart", "npp", "nvcv", "cudnn", "local", "pose", "trt", "feature"
            ]
        );
        // Every node comes after its dependencies.
        for (node, list) in &deps {
            let at = order.iter().position(|n| n == node).unwrap();
            for d in list {
                assert!(
                    order.iter().position(|n| n == d).unwrap() < at,
                    "{d} before {node}"
                );
            }
        }
        assert_eq!(
            order,
            load_order(&["nvcv", "local", "pose", "feature"], &deps)
        );
    }

    #[test]
    fn cycles_and_repeats_are_tolerated() {
        let deps = map(&[("a", &["b"]), ("b", &["c"]), ("c", &["a", "c"])]);
        assert_eq!(load_order(&["a", "a", "b"], &deps), ["c", "b", "a"]);
        assert_eq!(load_order::<&str>(&[], &deps), Vec::<&str>::new());
        // A root with no entry has no dependencies.
        assert_eq!(load_order(&["x"], &BTreeMap::new()), ["x"]);
    }

    #[test]
    fn the_needed_names_of_this_test_binary_and_of_a_system_library() {
        // The test binary links the C library dynamically.
        let exe = std::env::current_exe().unwrap();
        let names = needed_of_file(&exe);
        assert!(names.iter().any(|n| n.starts_with("libc.so")), "{names:?}");
        // The C library's own file, found through the loader: it needs the
        // dynamic loader and nothing else.
        let libc_path = loaded_object("libc.so").expect("libc in /proc/self/maps");
        let names = needed_of_file(&libc_path);
        assert!(
            names.iter().any(|n| n.starts_with("ld-linux")),
            "{}: {names:?}",
            libc_path.display()
        );
    }

    /// The file of the first loaded object whose name starts with `prefix`.
    fn loaded_object(prefix: &str) -> Option<PathBuf> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        maps.lines()
            .filter_map(|l| l.split_whitespace().nth(5))
            .map(PathBuf::from)
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
            })
    }

    #[test]
    fn anything_but_an_elf64_le_object_has_no_names() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("libtext.so");
        std::fs::write(&text, b"not a library").unwrap();
        assert!(needed_of_file(&text).is_empty());
        assert!(needed_of_file(&dir.path().join("absent.so")).is_empty());
        assert!(needed_of_bytes(b"").is_empty());
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let good = needed_of_bytes(&exe);
        assert!(!good.is_empty());
        // Another class or byte order.
        let mut other = exe.clone();
        other[4] = 1;
        assert!(needed_of_bytes(&other).is_empty());
        let mut other = exe.clone();
        other[5] = 2;
        assert!(needed_of_bytes(&other).is_empty());
        // Truncated anywhere: the header, the program headers, the
        // dynamic section. Never a panic; at most the names whose bytes
        // are there.
        for cut in [10, 64, 100, 1000, 4096, exe.len() / 2] {
            let names = needed_of_bytes(&exe[..cut.min(exe.len())]);
            assert!(names.iter().all(|n| good.contains(n)), "{cut}: {names:?}");
        }
        // Program headers pointing outside the file.
        let mut bad = exe.clone();
        bad[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(needed_of_bytes(&bad).is_empty());
        let mut bad = exe.clone();
        bad[0x38..0x3a].copy_from_slice(&PN_XNUM.to_le_bytes());
        assert!(needed_of_bytes(&bad).is_empty());
        // Garbage after a valid header.
        let mut junk = exe[..EHDR_SIZE].to_vec();
        junk.extend(std::iter::repeat_n(0xff, 4096));
        let _ = needed_of_bytes(&junk);
    }

    #[test]
    fn a_root_without_libraries_loads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(plan(dir.path(), CORE, FX).is_empty());
        let done = preload(dir.path(), CORE, FX);
        assert_eq!(done, Preloaded::default());
        assert_eq!(done.failures(), None);
        // Directories alone, and files that are not libraries, change
        // nothing.
        for d in ["lib", "features/a/lib", "external/cuda/lib"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        std::fs::write(dir.path().join("features/a/lib/README"), b"").unwrap();
        assert!(plan(dir.path(), CORE, FX).is_empty());
        assert!(search_dirs(dir.path()).contains(&dir.path().join("features/a/lib")));
    }

    #[test]
    fn a_root_library_that_does_not_load_is_recorded_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(lib.join("libNVCVImage.so.1"), b"not a library").unwrap();
        let feat = dir.path().join("features/fx/lib");
        std::fs::create_dir_all(&feat).unwrap();
        std::fs::write(feat.join("libnvARFx.so"), b"not a library either").unwrap();
        let plan = plan(dir.path(), CORE, FX);
        let canon = |p: PathBuf| p.canonicalize().unwrap();
        assert_eq!(
            plan,
            [
                canon(lib.join("libNVCVImage.so.1")),
                canon(feat.join("libnvARFx.so"))
            ]
        );
        let done = preload(dir.path(), CORE, FX);
        assert!(done.loaded.is_empty());
        assert_eq!(done.failed.len(), 2);
        let m = done.failures().unwrap();
        assert!(m.contains("libNVCVImage.so.1: "), "{m}");
        assert!(m.contains("libnvARFx.so: "), "{m}");
    }

    /// A minimal ELF64 little-endian shared object needing `names`: the
    /// header, a `PT_LOAD` mapping the whole file at address 0, a
    /// `PT_DYNAMIC`, the string table, then the dynamic section
    /// (`DT_NEEDED` each, `DT_STRTAB`, `DT_STRSZ`, `DT_NULL`). The offsets
    /// of the fields the crafted cases change are kept.
    struct Elf {
        bytes: Vec<u8>,
        /// The dynamic section's file offset.
        dynamic: usize,
        names: usize,
    }

    /// The program headers: `PT_LOAD` then `PT_DYNAMIC`.
    const LOAD_PH: usize = EHDR_SIZE;
    const DYNAMIC_PH: usize = EHDR_SIZE + PHDR_SIZE;

    impl Elf {
        fn new(names: &[&str]) -> Elf {
            let mut strtab = vec![0u8];
            let mut offsets = Vec::new();
            for n in names {
                offsets.push(strtab.len() as u64);
                strtab.extend_from_slice(n.as_bytes());
                strtab.push(0);
            }
            let str_at = EHDR_SIZE + 2 * PHDR_SIZE;
            let dynamic = (str_at + strtab.len()).next_multiple_of(8);
            let entries = names.len() + 3;
            let len = dynamic + entries * DYN_SIZE;
            let mut b = vec![0u8; len];
            b[..4].copy_from_slice(ELF_MAGIC);
            b[4] = ELFCLASS64;
            b[5] = ELFDATA2LSB;
            b[6] = 1;
            b[0x10..0x12].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
            b[0x12..0x14].copy_from_slice(&0x3eu16.to_le_bytes()); // x86-64
            b[0x20..0x28].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
            b[0x36..0x38].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
            b[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
            let mut elf = Elf {
                bytes: b,
                dynamic,
                names: names.len(),
            };
            elf.set32(LOAD_PH, PT_LOAD);
            elf.set(LOAD_PH + 32, len as u64);
            elf.set32(DYNAMIC_PH, PT_DYNAMIC);
            elf.set(DYNAMIC_PH + 8, dynamic as u64);
            elf.set(DYNAMIC_PH + 32, (entries * DYN_SIZE) as u64);
            elf.bytes[str_at..str_at + strtab.len()].copy_from_slice(&strtab);
            for (i, off) in offsets.iter().enumerate() {
                elf.entry(i, DT_NEEDED, *off);
            }
            elf.entry(names.len(), DT_STRTAB, str_at as u64);
            elf.entry(names.len() + 1, DT_STRSZ, strtab.len() as u64);
            elf.entry(names.len() + 2, DT_NULL, 0);
            elf
        }

        fn set(&mut self, at: usize, v: u64) {
            self.bytes[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }

        fn set32(&mut self, at: usize, v: u32) {
            self.bytes[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn entry(&mut self, i: usize, tag: u64, val: u64) {
            let at = self.dynamic + i * DYN_SIZE;
            self.set(at, tag);
            self.set(at + 8, val);
        }

        /// The value of `DT_NEEDED` `i`.
        fn needed_at(&self, i: usize) -> usize {
            self.dynamic + i * DYN_SIZE + 8
        }

        fn strtab_at(&self) -> usize {
            self.dynamic + self.names * DYN_SIZE + 8
        }

        fn strsz_at(&self) -> usize {
            self.dynamic + (self.names + 1) * DYN_SIZE + 8
        }

        fn names(&self) -> Vec<String> {
            needed_of_bytes(&self.bytes)
        }
    }

    const NAMES: [&str; 2] = ["libcudart.so.12", "libc.so.6"];

    #[test]
    fn a_crafted_object_has_the_names_it_lists() {
        let elf = Elf::new(&NAMES);
        assert_eq!(elf.names(), NAMES);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libx.so");
        std::fs::write(&path, &elf.bytes).unwrap();
        assert_eq!(needed_of_file(&path), NAMES);
        assert!(Elf::new(&[]).names().is_empty());
    }

    #[test]
    fn a_string_table_outside_every_load_segment_has_no_names() {
        // Past the end of the only segment.
        let mut elf = Elf::new(&NAMES);
        let at = elf.strtab_at();
        elf.set(at, 1 << 40);
        assert!(elf.names().is_empty());
        let mut elf = Elf::new(&NAMES);
        elf.set(at, u64::MAX);
        assert!(elf.names().is_empty());
        // The segment ends before the string table.
        let mut elf = Elf::new(&NAMES);
        elf.set(LOAD_PH + 32, 8);
        assert!(elf.names().is_empty());
        // The segment starts above it, or is not a load segment at all.
        let mut elf = Elf::new(&NAMES);
        elf.set(LOAD_PH + 16, u64::MAX);
        assert!(elf.names().is_empty());
        let mut elf = Elf::new(&NAMES);
        elf.set32(LOAD_PH, 4); // PT_NOTE
        assert!(elf.names().is_empty());
        // No string table entry at all.
        let mut elf = Elf::new(&NAMES);
        let tag = elf.strtab_at() - 8;
        elf.set(tag, 0x6fff_fff0);
        assert!(elf.names().is_empty());
    }

    #[test]
    fn huge_sizes_and_offsets_are_bounded_never_a_panic() {
        // A huge string table size: bounded by the segment, the names read.
        let mut elf = Elf::new(&NAMES);
        let at = elf.strsz_at();
        elf.set(at, u64::MAX);
        assert_eq!(elf.names(), NAMES);
        // A zero one: no name is inside it.
        elf.set(at, 0);
        assert!(elf.names().is_empty());
        // A needed name far outside the table is skipped, the others read.
        let mut elf = Elf::new(&NAMES);
        let at = elf.needed_at(0);
        elf.set(at, u64::MAX);
        assert_eq!(elf.names(), ["libc.so.6"]);
        elf.set(at, (1 << 32) + 1);
        assert_eq!(elf.names(), ["libc.so.6"]);
        // A load segment mapped from the end of the address space of file
        // offsets: the name's offset overflows, nothing is read.
        let mut elf = Elf::new(&NAMES);
        elf.set(LOAD_PH + 8, u64::MAX - 4);
        assert!(elf.names().is_empty());
        // A load segment claiming more file bytes than there are: the names
        // inside the file still read, as far as the file goes.
        let mut elf = Elf::new(&NAMES);
        elf.set(LOAD_PH + 32, u64::MAX);
        let names = elf.names();
        assert!(
            names.iter().all(|n| NAMES.contains(&n.as_str())),
            "{names:?}"
        );
        // The dynamic section anywhere, or of any size.
        for (field, v) in [(8, u64::MAX), (8, 1 << 40), (32, u64::MAX), (32, 1 << 40)] {
            let mut elf = Elf::new(&NAMES);
            elf.set(DYNAMIC_PH + field, v);
            assert!(elf.names().is_empty(), "{field} = {v}");
        }
        // Program headers: too many for the file, an entry size too small
        // or too large, a table offset that overflows.
        for (at, v) in [(0x38usize, 0xfffeu16), (0x36, 8), (0x36, 0x1000)] {
            let mut elf = Elf::new(&NAMES);
            elf.bytes[at..at + 2].copy_from_slice(&v.to_le_bytes());
            assert!(elf.names().is_empty(), "{at:#x} = {v}");
        }
        let mut elf = Elf::new(&NAMES);
        elf.set(0x20, u64::MAX - 8);
        assert!(elf.names().is_empty());
    }

    #[test]
    fn a_truncated_dynamic_section_has_no_names() {
        let elf = Elf::new(&NAMES);
        // Cut inside the dynamic section: it cannot be read whole.
        for cut in [
            elf.dynamic,
            elf.dynamic + 1,
            elf.dynamic + DYN_SIZE,
            elf.bytes.len() - 1,
        ] {
            assert!(needed_of_bytes(&elf.bytes[..cut]).is_empty(), "{cut}");
        }
        // Cut anywhere at all: never a panic, never a name that is not there.
        for cut in 0..elf.bytes.len() {
            let names = needed_of_bytes(&elf.bytes[..cut]);
            assert!(
                names.iter().all(|n| NAMES.contains(&n.as_str())),
                "{cut}: {names:?}"
            );
        }
        // A section that ends without DT_NULL is read to its end.
        let mut elf = Elf::new(&NAMES);
        let last = NAMES.len() + 2;
        elf.entry(last, DT_STRSZ, 64);
        let names = elf.names();
        assert!(
            names.iter().all(|n| NAMES.contains(&n.as_str())),
            "{names:?}"
        );
        // A name running to the end of the file without its NUL is skipped.
        let mut elf = Elf::new(&["libonly.so"]);
        let str_at = EHDR_SIZE + 2 * PHDR_SIZE;
        elf.bytes.truncate(str_at + 4);
        elf.set(LOAD_PH + 32, elf.bytes.len() as u64);
        assert!(elf.names().is_empty());
    }

    /// Writes a crafted object needing `needs` at `root/rel`.
    fn object(root: &Path, rel: &str, needs: &[&str]) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, Elf::new(needs).bytes).unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn a_fixture_root_loads_dependencies_first() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let nvcv = object(
            r,
            "lib/libNVCVImage.so.1",
            &["libnppc.so.12", "libcudart.so.12", "libc.so.6"],
        );
        std::os::unix::fs::symlink("libNVCVImage.so.1", r.join("lib/libNVCVImage.so")).unwrap();
        let local = object(
            r,
            "lib/libnvARPoseLocal.so",
            &["libNVCVImage.so", "libcudnn.so.9"],
        );
        let pose = object(r, "lib/libnvARPose.so", &["libNVCVImage.so"]);
        let nppc = object(r, "external/cuda/lib/libnppc.so.12", &["libcudart.so.12"]);
        let cudart = object(r, "external/cuda/lib/libcudart.so.12", &["libc.so.6"]);
        let cudnn = object(r, "external/cuda/lib/libcudnn.so.9", &[]);
        let jit = object(r, "external/cuda/lib/libnvJitLink.so.12", &["libc.so.6"]);
        let trt = object(
            r,
            "external/tensorrt/lib/libnvinfer.so.10",
            &["libcudart.so.12"],
        );
        let fx = object(
            r,
            "features/fx/lib/libnvARFx.so",
            &["libnvARPoseLocal.so", "libnvinfer.so.10"],
        );
        // Bundled but needed by nothing: not loaded.
        object(r, "external/cuda/lib/libcufft.so.11", &[]);
        object(r, "lib/libnvARPoseTriton.so", &["libNVCVImage.so"]);
        // A feature the module does not use: not loaded.
        object(
            r,
            "features/other/lib/libnvAROther.so",
            &["libNVCVImage.so"],
        );
        let roots: Vec<PathBuf> = roots(r, CORE, FX)
            .into_iter()
            .map(|p| p.canonicalize().unwrap())
            .collect();
        assert_eq!(
            roots,
            [
                nvcv.clone(),
                local.clone(),
                pose.clone(),
                jit.clone(),
                fx.clone()
            ]
        );
        assert_eq!(
            plan(r, CORE, FX),
            [cudart, nppc, nvcv, cudnn, local, pose, jit, trt, fx]
        );
        // Without the JIT linker the rest is unchanged.
        std::fs::remove_file(r.join("external/cuda/lib/libnvJitLink.so.12")).unwrap();
        let plan = plan(r, CORE, FX);
        assert_eq!(plan.len(), 8);
        assert!(!plan.iter().any(|p| p.ends_with("libnvJitLink.so.12")));
    }

    #[test]
    fn needed_names_resolve_in_the_sdk_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = search_dirs(dir.path());
        let cuda = dir.path().join("external/cuda/lib");
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&cuda).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(cuda.join("libcudart.so.12"), b"").unwrap();
        std::fs::write(cuda.join("libdup.so"), b"").unwrap();
        std::fs::write(lib.join("libdup.so"), b"").unwrap();
        assert_eq!(
            resolve("libcudart.so.12", &dirs),
            Some(cuda.join("libcudart.so.12").canonicalize().unwrap())
        );
        // The first directory that has it wins: lib before external.
        assert_eq!(
            resolve("libdup.so", &dirs),
            Some(lib.join("libdup.so").canonicalize().unwrap())
        );
        // Not in the SDK: the system's.
        assert_eq!(resolve("libc.so.6", &dirs), None);
        assert_eq!(resolve("../lib/libdup.so", &dirs), None);
        assert_eq!(resolve("", &dirs), None);
    }
}
