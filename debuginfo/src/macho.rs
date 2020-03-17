//! Support for Mach Objects, used on macOS and iOS.

use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;

use failure::Fail;
use goblin::{error::Error as GoblinError, mach};
use smallvec::SmallVec;

use symbolic_common::{Arch, AsSelf, CodeId, DebugId, Uuid};

use crate::base::*;
use crate::dwarf::{Dwarf, DwarfDebugSession, DwarfError, DwarfSection, Endian};
use crate::private::{MonoArchive, MonoArchiveObjects, Parse};

/// An error when dealing with [`MachObject`](struct.MachObject.html).
#[derive(Debug, Fail)]
pub enum MachError {
    /// The data in the MachO file could not be parsed.
    #[fail(display = "invalid MachO file")]
    BadObject(#[fail(cause)] GoblinError),
}

/// Mach Object containers, used for executables and debug companions on macOS and iOS.
pub struct MachObject<'d> {
    macho: mach::MachO<'d>,
    data: &'d [u8],
}

impl<'d> MachObject<'d> {
    /// Tests whether the buffer could contain a MachO object.
    pub fn test(data: &[u8]) -> bool {
        match goblin::peek(&mut Cursor::new(data)) {
            Ok(goblin::Hint::Mach(_)) => true,
            _ => false,
        }
    }

    /// Tries to parse a MachO from the given slice.
    pub fn parse(data: &'d [u8]) -> Result<Self, MachError> {
        mach::MachO::parse(data, 0)
            .map(|macho| MachObject { macho, data })
            .map_err(MachError::BadObject)
    }

    /// The container file format, which is always `FileFormat::MachO`.
    pub fn file_format(&self) -> FileFormat {
        FileFormat::MachO
    }

    fn find_uuid(&self) -> Option<Uuid> {
        for cmd in &self.macho.load_commands {
            if let mach::load_command::CommandVariant::Uuid(ref uuid_cmd) = cmd.command {
                return Uuid::from_slice(&uuid_cmd.uuid).ok();
            }
        }

        None
    }

    /// The code identifier of this object.
    ///
    /// Mach objects use a UUID which is specified in the load commands that are part of the Mach
    /// header. This UUID is generated at compile / link time and is usually unique per compilation.
    pub fn code_id(&self) -> Option<CodeId> {
        let uuid = self.find_uuid()?;
        Some(CodeId::from_binary(&uuid.as_bytes()[..]))
    }

    /// The debug information identifier of a MachO file.
    ///
    /// This uses the same UUID as `code_id`.
    pub fn debug_id(&self) -> DebugId {
        self.find_uuid().map(DebugId::from_uuid).unwrap_or_default()
    }

    /// The CPU architecture of this object, as specified in the Mach header.
    pub fn arch(&self) -> Arch {
        use goblin::mach::constants::cputype;

        match (self.macho.header.cputype(), self.macho.header.cpusubtype()) {
            (cputype::CPU_TYPE_I386, cputype::CPU_SUBTYPE_I386_ALL) => Arch::X86,
            (cputype::CPU_TYPE_I386, _) => Arch::X86Unknown,
            (cputype::CPU_TYPE_X86_64, cputype::CPU_SUBTYPE_X86_64_ALL) => Arch::Amd64,
            (cputype::CPU_TYPE_X86_64, cputype::CPU_SUBTYPE_X86_64_H) => Arch::Amd64h,
            (cputype::CPU_TYPE_X86_64, _) => Arch::Amd64Unknown,
            (cputype::CPU_TYPE_ARM64, cputype::CPU_SUBTYPE_ARM64_ALL) => Arch::Arm64,
            (cputype::CPU_TYPE_ARM64, cputype::CPU_SUBTYPE_ARM64_V8) => Arch::Arm64V8,
            (cputype::CPU_TYPE_ARM64, cputype::CPU_SUBTYPE_ARM64_E) => Arch::Arm64e,
            (cputype::CPU_TYPE_ARM64, _) => Arch::Arm64Unknown,
            (cputype::CPU_TYPE_ARM64_32, cputype::CPU_SUBTYPE_ARM64_32_ALL) => Arch::Arm64_32,
            (cputype::CPU_TYPE_ARM64_32, cputype::CPU_SUBTYPE_ARM64_32_V8) => Arch::Arm64_32V8,
            (cputype::CPU_TYPE_ARM64_32, _) => Arch::Arm64_32Unknown,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_ALL) => Arch::Arm,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V5TEJ) => Arch::ArmV5,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V6) => Arch::ArmV6,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V6M) => Arch::ArmV6m,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7) => Arch::ArmV7,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7F) => Arch::ArmV7f,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7S) => Arch::ArmV7s,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7K) => Arch::ArmV7k,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7M) => Arch::ArmV7m,
            (cputype::CPU_TYPE_ARM, cputype::CPU_SUBTYPE_ARM_V7EM) => Arch::ArmV7em,
            (cputype::CPU_TYPE_ARM, _) => Arch::ArmUnknown,
            (cputype::CPU_TYPE_POWERPC, cputype::CPU_SUBTYPE_POWERPC_ALL) => Arch::Ppc,
            (cputype::CPU_TYPE_POWERPC64, cputype::CPU_SUBTYPE_POWERPC_ALL) => Arch::Ppc64,
            (_, _) => Arch::Unknown,
        }
    }

    /// The kind of this object, as specified in the Mach header.
    pub fn kind(&self) -> ObjectKind {
        match self.macho.header.filetype {
            goblin::mach::header::MH_OBJECT => ObjectKind::Relocatable,
            goblin::mach::header::MH_EXECUTE => ObjectKind::Executable,
            goblin::mach::header::MH_FVMLIB => ObjectKind::Library,
            goblin::mach::header::MH_CORE => ObjectKind::Dump,
            goblin::mach::header::MH_PRELOAD => ObjectKind::Executable,
            goblin::mach::header::MH_DYLIB => ObjectKind::Library,
            goblin::mach::header::MH_DYLINKER => ObjectKind::Executable,
            goblin::mach::header::MH_BUNDLE => ObjectKind::Library,
            goblin::mach::header::MH_DSYM => ObjectKind::Debug,
            goblin::mach::header::MH_KEXT_BUNDLE => ObjectKind::Library,
            _ => ObjectKind::Other,
        }
    }

    /// The address at which the image prefers to be loaded into memory.
    ///
    /// MachO files store all internal addresses as if it was loaded at that address. When the image
    /// is actually loaded, that spot might already be taken by other images and so it must be
    /// relocated to a new address. At runtime, a relocation table manages the arithmetics behind
    /// this.
    ///
    /// Addresses used in `symbols` or `debug_session` have already been rebased relative to that
    /// load address, so that the caller only has to deal with addresses relative to the actual
    /// start of the image.
    pub fn load_address(&self) -> u64 {
        for seg in &self.macho.segments {
            if seg.name().map(|name| name == "__TEXT").unwrap_or(false) {
                return seg.vmaddr;
            }
        }

        0
    }

    /// Determines whether this object exposes a public symbol table.
    pub fn has_symbols(&self) -> bool {
        self.macho.symbols.is_some()
    }

    /// Returns an iterator over symbols in the public symbol table.
    pub fn symbols(&self) -> MachOSymbolIterator<'d> {
        // Cache indices of code sections. These are either "__text" or "__stubs", always located in
        // the "__TEXT" segment. It looks like each of those sections only occurs once, but to be
        // safe they are collected into a vector.
        let mut sections = SmallVec::new();
        let mut section_index = 0;

        'outer: for segment in &self.macho.segments {
            if segment.name().ok() != Some("__TEXT") {
                section_index += segment.nsects as usize;
                continue;
            }

            for result in segment {
                // Do not continue to iterate potentially broken section headers. This could lead to
                // invalid section indices.
                let section = match result {
                    Ok((section, _data)) => section,
                    Err(_) => break 'outer,
                };

                match section.name() {
                    Ok("__text") | Ok("__stubs") => sections.push(section_index),
                    _ => (),
                }

                section_index += 1;
            }
        }

        MachOSymbolIterator {
            symbols: self.macho.symbols(),
            sections,
            vmaddr: self.load_address(),
        }
    }

    /// Returns an ordered map of symbols in the symbol table.
    pub fn symbol_map(&self) -> SymbolMap<'d> {
        self.symbols().collect()
    }

    /// Determines whether this object contains debug information.
    pub fn has_debug_info(&self) -> bool {
        self.has_section("debug_info")
    }

    /// Constructs a debugging session.
    ///
    /// A debugging session loads certain information from the object file and creates caches for
    /// efficient access to various records in the debug information. Since this can be quite a
    /// costly process, try to reuse the debugging session as long as possible.
    ///
    /// MachO files generally use DWARF debugging information, which is also used by ELF containers
    /// on Linux.
    ///
    /// Constructing this session will also work if the object does not contain debugging
    /// information, in which case the session will be a no-op. This can be checked via
    /// [`has_debug_info`](struct.MachObject.html#method.has_debug_info).
    pub fn debug_session(&self) -> Result<DwarfDebugSession<'d>, DwarfError> {
        let symbols = self.symbol_map();
        DwarfDebugSession::parse(self, symbols, self.load_address())
    }

    /// Determines whether this object contains stack unwinding information.
    pub fn has_unwind_info(&self) -> bool {
        self.has_section("eh_frame") || self.has_section("debug_frame")
    }

    /// Determines whether this object contains embedded source.
    pub fn has_sources(&self) -> bool {
        false
    }

    /// Returns the raw data of the ELF file.
    pub fn data(&self) -> &'d [u8] {
        self.data
    }

    /// Checks whether this mach object contains hidden symbols.
    ///
    /// This is an indication that BCSymbolMaps are needed to symbolicate crash reports correctly.
    pub fn requires_symbolmap(&self) -> bool {
        self.symbols()
            .any(|s| s.name().map_or(false, |n| n.starts_with("__?hidden#")))
    }
}

impl fmt::Debug for MachObject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MachObject")
            .field("code_id", &self.code_id())
            .field("debug_id", &self.debug_id())
            .field("arch", &self.arch())
            .field("kind", &self.kind())
            .field("load_address", &format_args!("{:#x}", self.load_address()))
            .field("has_symbols", &self.has_symbols())
            .field("has_debug_info", &self.has_debug_info())
            .field("has_unwind_info", &self.has_unwind_info())
            .finish()
    }
}

impl<'slf, 'd: 'slf> AsSelf<'slf> for MachObject<'d> {
    type Ref = MachObject<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        self
    }
}

impl<'d> Parse<'d> for MachObject<'d> {
    type Error = MachError;

    fn test(data: &[u8]) -> bool {
        Self::test(data)
    }

    fn parse(data: &'d [u8]) -> Result<Self, MachError> {
        Self::parse(data)
    }
}

impl<'d> ObjectLike for MachObject<'d> {
    type Error = DwarfError;
    type Session = DwarfDebugSession<'d>;

    fn file_format(&self) -> FileFormat {
        self.file_format()
    }

    fn code_id(&self) -> Option<CodeId> {
        self.code_id()
    }

    fn debug_id(&self) -> DebugId {
        self.debug_id()
    }

    fn arch(&self) -> Arch {
        self.arch()
    }

    fn kind(&self) -> ObjectKind {
        self.kind()
    }

    fn load_address(&self) -> u64 {
        self.load_address()
    }

    fn has_symbols(&self) -> bool {
        self.has_symbols()
    }

    fn symbols(&self) -> DynIterator<'_, Symbol<'_>> {
        Box::new(self.symbols())
    }

    fn symbol_map(&self) -> SymbolMap<'_> {
        self.symbol_map()
    }

    fn has_debug_info(&self) -> bool {
        self.has_debug_info()
    }

    fn debug_session(&self) -> Result<Self::Session, Self::Error> {
        self.debug_session()
    }

    fn has_unwind_info(&self) -> bool {
        self.has_unwind_info()
    }

    fn has_sources(&self) -> bool {
        self.has_sources()
    }
}

impl<'d> Dwarf<'d> for MachObject<'d> {
    fn endianity(&self) -> Endian {
        if self.macho.little_endian {
            Endian::Little
        } else {
            Endian::Big
        }
    }

    fn raw_section(&self, section_name: &str) -> Option<DwarfSection<'d>> {
        for segment in &self.macho.segments {
            for section in segment {
                if let Ok((header, data)) = section {
                    if let Ok(sec) = header.name() {
                        if sec.len() >= 2 && &sec[2..] == section_name {
                            // In some cases, dsymutil leaves sections headers but removes their
                            // data from the file. While the addr and size parameters are still
                            // set, `header.offset` is 0 in that case. We skip them just like the
                            // section was missing to avoid loading invalid data.
                            if header.offset == 0 {
                                return None;
                            }

                            return Some(DwarfSection {
                                data: Cow::Borrowed(data),
                                address: header.addr,
                                offset: u64::from(header.offset),
                                align: u64::from(header.align),
                            });
                        }
                    }
                }
            }
        }

        None
    }
}

/// An iterator over symbols in the MachO file.
///
/// Returned by [`MachObject::symbols`](struct.MachObject.html#method.symbols).
pub struct MachOSymbolIterator<'d> {
    symbols: mach::symbols::SymbolIterator<'d>,
    sections: SmallVec<[usize; 2]>,
    vmaddr: u64,
}

impl<'d> Iterator for MachOSymbolIterator<'d> {
    type Item = Symbol<'d>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(next) = self.symbols.next() {
            // Gracefully recover from corrupt nlists
            let (mut name, nlist) = match next {
                Ok(pair) => pair,
                Err(_) => continue,
            };

            // Sanity check of the symbol address. Since we only intend to iterate over function
            // symbols, they need to be mapped after the image's vmaddr.
            if nlist.n_value < self.vmaddr {
                continue;
            }

            // We are only interested in symbols pointing to a code section (type `N_SECT`). The
            // section index is incremented by one to leave room for `NO_SECT` (0). Section indexes
            // of the code sections have been passed in via `self.sections`.
            let in_valid_section = !nlist.is_stab()
                && nlist.get_type() == mach::symbols::N_SECT
                && nlist.n_sect != (mach::symbols::NO_SECT as usize)
                && self.sections.contains(&(nlist.n_sect - 1));

            if !in_valid_section {
                continue;
            }

            // Trim leading underscores from mangled C++ names.
            if name.starts_with('_') {
                name = &name[1..];
            }

            return Some(Symbol {
                name: Some(Cow::Borrowed(name)),
                address: nlist.n_value - self.vmaddr,
                size: 0, // Computed in `SymbolMap`
            });
        }

        None
    }
}

/// An iterator over objects in a [`FatMachO`](struct.FatMachO.html).
///
/// Objects are parsed just-in-time while iterating, which may result in errors. The iterator is
/// still valid afterwards, however, and can be used to resolve the next object.
pub struct FatMachObjectIterator<'d, 'a> {
    iter: mach::FatArchIterator<'a>,
    remaining: usize,
    data: &'d [u8],
}

impl<'d, 'a> Iterator for FatMachObjectIterator<'d, 'a> {
    type Item = Result<MachObject<'d>, MachError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }

        self.remaining -= 1;
        match self.iter.next() {
            Some(Ok(arch)) => {
                let start = (arch.offset as usize).min(self.data.len());
                let end = ((arch.offset + arch.size) as usize).min(self.data.len());
                Some(MachObject::parse(&self.data[start..end]))
            }
            Some(Err(error)) => Some(Err(MachError::BadObject(error))),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl std::iter::FusedIterator for FatMachObjectIterator<'_, '_> {}
impl ExactSizeIterator for FatMachObjectIterator<'_, '_> {}

/// A fat MachO container that hosts one or more [`MachObject`]s.
///
/// [`MachObject`]: struct.MachObject.html
pub struct FatMachO<'d> {
    fat: mach::MultiArch<'d>,
    data: &'d [u8],
}

impl<'d> FatMachO<'d> {
    /// Tests whether the buffer could contain an ELF object.
    pub fn test(data: &[u8]) -> bool {
        match goblin::peek(&mut Cursor::new(data)) {
            Ok(goblin::Hint::MachFat(_)) => true,
            _ => false,
        }
    }

    /// Tries to parse a fat MachO container from the given slice.
    pub fn parse(data: &'d [u8]) -> Result<Self, MachError> {
        mach::MultiArch::new(data)
            .map(|fat| FatMachO { fat, data })
            .map_err(MachError::BadObject)
    }

    /// Returns an iterator over objects in this container.
    pub fn objects(&self) -> FatMachObjectIterator<'d, '_> {
        FatMachObjectIterator {
            iter: self.fat.iter_arches(),
            remaining: self.fat.narches,
            data: self.data,
        }
    }

    /// Returns the number of objects in this archive.
    pub fn object_count(&self) -> usize {
        self.fat.narches
    }

    /// Resolves the object at the given index.
    ///
    /// Returns `Ok(None)` if the index is out of bounds, or `Err` if the object exists but cannot
    /// be parsed.
    pub fn object_by_index(&self, index: usize) -> Result<Option<MachObject<'d>>, MachError> {
        let arch = match self.fat.iter_arches().nth(index) {
            Some(arch) => arch.map_err(MachError::BadObject)?,
            None => return Ok(None),
        };

        let start = (arch.offset as usize).min(self.data.len());
        let end = ((arch.offset + arch.size) as usize).min(self.data.len());
        MachObject::parse(&self.data[start..end]).map(Some)
    }
}

impl fmt::Debug for FatMachO<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FatMachO").field("fat", &self.fat).finish()
    }
}

impl<'slf, 'd: 'slf> AsSelf<'slf> for FatMachO<'d> {
    type Ref = FatMachO<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        self
    }
}

#[allow(clippy::large_enum_variant)]
enum MachObjectIteratorInner<'d, 'a> {
    Single(MonoArchiveObjects<'d, MachObject<'d>>),
    Archive(FatMachObjectIterator<'d, 'a>),
}

/// An iterator over objects in a [`MachArchive`](struct.MachArchive.html).
pub struct MachObjectIterator<'d, 'a>(MachObjectIteratorInner<'d, 'a>);

impl<'d, 'a> Iterator for MachObjectIterator<'d, 'a> {
    type Item = Result<MachObject<'d>, MachError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.0 {
            MachObjectIteratorInner::Single(ref mut iter) => iter.next(),
            MachObjectIteratorInner::Archive(ref mut iter) => iter.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.0 {
            MachObjectIteratorInner::Single(ref iter) => iter.size_hint(),
            MachObjectIteratorInner::Archive(ref iter) => iter.size_hint(),
        }
    }
}

impl std::iter::FusedIterator for MachObjectIterator<'_, '_> {}
impl ExactSizeIterator for MachObjectIterator<'_, '_> {}

#[derive(Debug)]
enum MachArchiveInner<'d> {
    Single(MonoArchive<'d, MachObject<'d>>),
    Archive(FatMachO<'d>),
}

/// An archive that can consist of a single [`MachObject`] or a [`FatMachO`] container.
///
/// Executables and dSYM files on macOS can be a so-called _Fat Mach Object_: It contains multiple
/// objects for several architectures. When loading this object, the operating system determines the
/// object corresponding to the host's architecture. This allows to distribute a single binary with
/// optimizations for specific CPUs, which is frequently done on iOS.
///
/// To abstract over the differences, `MachArchive` simulates the archive interface also for single
/// Mach objects. This allows uniform access to both file types.
///
/// [`MachObject`]: struct.MachObject.html
/// [`FatMachO`]: struct.FatMachO.html
#[derive(Debug)]
pub struct MachArchive<'d>(MachArchiveInner<'d>);

impl<'d> MachArchive<'d> {
    /// Tests whether the buffer contains either a Mach Object or a Fat Mach Object.
    pub fn test(data: &[u8]) -> bool {
        match goblin::peek(&mut Cursor::new(data)) {
            Ok(goblin::Hint::Mach(_)) => true,
            Ok(goblin::Hint::MachFat(narchs)) => {
                // so this is kind of stupid but java class files share the same cutsey magic
                // as a macho fat file (CAFEBABE).  This means that we often claim that a java
                // class file is actually a macho binary but it's not.  The next 32 bytes encode
                // the number of embedded architectures in a fat mach.  In case of a JAR file
                // we have 2 bytes for minor version and 2 bytes for major version of the class
                // file format.
                //
                // The internet suggests the first public version of Java had the class version
                // 45.  Thus the logic applied here is that if the number is >= 45 we're more
                // likely to have a java class file than a macho file with 45 architectures
                // which should be very rare.
                //
                // https://docs.oracle.com/javase/specs/jvms/se6/html/ClassFile.doc.html
                narchs < 45
            }
            _ => false,
        }
    }

    /// Tries to parse a Mach archive from the given slice.
    pub fn parse(data: &'d [u8]) -> Result<Self, MachError> {
        Ok(MachArchive(match goblin::peek(&mut Cursor::new(data)) {
            Ok(goblin::Hint::MachFat(_)) => MachArchiveInner::Archive(FatMachO::parse(data)?),
            // Fall back to mach parsing to receive a meaningful error message from goblin
            _ => MachArchiveInner::Single(MonoArchive::new(data)),
        }))
    }

    /// Returns an iterator over all objects contained in this archive.
    pub fn objects(&self) -> MachObjectIterator<'d, '_> {
        MachObjectIterator(match self.0 {
            MachArchiveInner::Single(ref inner) => MachObjectIteratorInner::Single(inner.objects()),
            MachArchiveInner::Archive(ref inner) => {
                MachObjectIteratorInner::Archive(inner.objects())
            }
        })
    }

    /// Returns the number of objects in this archive.
    pub fn object_count(&self) -> usize {
        match self.0 {
            MachArchiveInner::Single(ref inner) => inner.object_count(),
            MachArchiveInner::Archive(ref inner) => inner.object_count(),
        }
    }

    /// Resolves the object at the given index.
    ///
    /// Returns `Ok(None)` if the index is out of bounds, or `Err` if the object exists but cannot
    /// be parsed.
    pub fn object_by_index(&self, index: usize) -> Result<Option<MachObject<'d>>, MachError> {
        match self.0 {
            MachArchiveInner::Single(ref inner) => inner.object_by_index(index),
            MachArchiveInner::Archive(ref inner) => inner.object_by_index(index),
        }
    }

    /// Returns whether this is a multi-object archive.
    ///
    /// This may also return true if there is only a single object inside the archive.
    pub fn is_multi(&self) -> bool {
        match self.0 {
            MachArchiveInner::Archive(_) => true,
            MachArchiveInner::Single(_) => false,
        }
    }
}

impl<'slf, 'd: 'slf> AsSelf<'slf> for MachArchive<'d> {
    type Ref = MachArchive<'slf>;

    fn as_self(&'slf self) -> &Self::Ref {
        self
    }
}
