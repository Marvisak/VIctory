//! Windows-native exports backed by VIctory's compatibility implementations.
//! Console DLL names forward here so all of them share runtime state.

#![allow(non_snake_case)]

#[cfg(windows)]
mod windows_exports {
    use std::ffi::{c_char, c_void};
    use std::fs::{File, OpenOptions};
    use std::os::windows::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf, Prefix};
    use std::sync::Once;
    use victory_gpu::{device, object, root};
    use victory_hle::modules::{
        apu, audio, d3d12x, dsp, dstorage, gameinput, net, xg, xgameruntime, xmem,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    use xmem::WorkingSet;

    /// Reject UNC, mapped-network drives, and junctions before file IO. The
    /// final component may not exist yet when a log or save folder is made.
    fn fixed_local_path(path: &Path, may_be_new: bool) -> bool {
        let Some(Component::Prefix(prefix)) = path.components().next() else { return false };
        let Prefix::Disk(letter) = prefix.kind() else { return false };
        if !path.is_absolute() { return false }
        let root = [u16::from(letter), u16::from(b':'), u16::from(b'\\'), 0];
        // SAFETY: root is a null-terminated, four-code-unit drive path.
        if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 { return false }
        let mut current = PathBuf::from(format!("{}:\\", char::from(letter)));
        for part in path.components().skip(2) {
            let Component::Normal(name) = part else { return false };
            current.push(name);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_attributes() & 0x400 == 0 => {}
                Err(error) if may_be_new && error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return false,
            }
        }
        true
    }

    fn local_log() -> Option<File> {
        let requested = std::env::var_os("VICTORY_NATIVE_LOG_PATH")
            .map(PathBuf::from)
            .filter(|path| fixed_local_path(path, true));
        let beside_exe = std::env::current_exe()
            .ok()
            .map(|path| path.with_file_name("victory-native.log"))
            .filter(|path| fixed_local_path(path, true));
        let in_user_data = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("victory").join("victory-native.log"))
            .filter(|path| fixed_local_path(path, true));
        for path in requested.into_iter().chain(beside_exe).chain(in_user_data) {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if !fixed_local_path(&path, true) { continue }
            if let Ok(file) = OpenOptions::new().create(true).append(true).open(path) {
                return Some(file);
            }
        }
        None
    }

    fn init_title_image() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            if let Some(file) = local_log() {
                let level = std::env::var("VICTORY_NATIVE_LOG_LEVEL")
                    .ok()
                    .and_then(|value| value.parse::<tracing::Level>().ok())
                    .unwrap_or(tracing::Level::INFO);
                let subscriber = tracing_subscriber::fmt()
                    .with_writer(file)
                    .with_ansi(false)
                    .with_max_level(level)
                    .finish();
                let _ = tracing::subscriber::set_global_default(subscriber);
            }
            match std::env::var("VICTORY_CONSOLE").as_deref() {
                Ok("series-s" | "s") => {
                    victory_hle::console::set(victory_hle::console::Console::SeriesS);
                }
                Ok("series-x" | "x") | Err(_) => {}
                Ok(value) => tracing::warn!(value, "invalid VICTORY_CONSOLE; using series-x"),
            }
            tracing::info!(console = victory_hle::console::current().name(), "console profile");
            // SAFETY: null requests the executable module in this process.
            let image = unsafe { GetModuleHandleW(std::ptr::null()) };
            if !image.is_null() {
                victory_hle::runtime::set_main_image(image as usize);
            }
            let manifest = [
                Some(PathBuf::from(r"G:\MicrosoftGame.Config")),
                std::env::current_exe()
                    .ok()
                    .map(|exe| exe.with_file_name("MicrosoftGame.Config")),
            ]
            .into_iter()
            .flatten()
            .filter(|path| fixed_local_path(path, false))
            .find_map(|path| std::fs::read_to_string(path).ok());
            let mut title = victory_hle::runtime::Title::from_manifest(
                manifest.as_deref().unwrap_or_default(),
            );
            if let Some(base) = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| fixed_local_path(path, false))
            {
                let storage = base
                    .join("victory")
                    .join("data")
                    .join(format!("{:08X}", title.id))
                    .join("storage");
                match std::fs::create_dir_all(&storage) {
                    Ok(()) if fixed_local_path(&storage, false) => title.storage = storage,
                    Err(error) => tracing::warn!(%error, path = %storage.display(), "native storage unavailable"),
                    Ok(()) => tracing::warn!(path = %storage.display(), "native storage path is not a fixed local folder"),
                }
            }
            tracing::info!(
                id = format_args!("{:08X}", title.id),
                storage = %title.storage.display(),
                storage_mb = title.storage_bytes >> 20,
                "native title storage"
            );
            victory_hle::runtime::set_title(title);
            tracing::info!("native compatibility runtime initialized");
        });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
        init_title_image();
        xmem::XMemAlloc(size, attributes)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
        init_title_image();
        xmem::XMemFree(address, attributes);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
        xmem::XMemAllocDefault(size, attributes)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
        xmem::XMemFreeDefault(address, attributes);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemIsToolsMemAvailable() -> i32 {
        xmem::XMemIsToolsMemAvailable()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemAllocatePhysicalPages(
        flags: u64,
        count: *mut usize,
        pages: *mut usize,
    ) -> i32 {
        xmem::XMemAllocatePhysicalPages(flags, count, pages)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> i32 {
        xmem::XMemFreePhysicalPages(n, pages)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemMapPhysicalPages(
        address: *mut c_void,
        n: usize,
        pages: *const usize,
    ) -> *mut c_void {
        xmem::XMemMapPhysicalPages(address, n, pages)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemMapPhysicalPagesScatter(
        addresses: *const *mut c_void,
        n: usize,
        pages: *const usize,
    ) -> i32 {
        xmem::XMemMapPhysicalPagesScatter(addresses, n, pages)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemVirtualAlloc(
        base: *mut c_void,
        size: usize,
        kind: u32,
        flags: u64,
        protect: u32,
    ) -> *mut c_void {
        xmem::XMemVirtualAlloc(base, size, kind, flags, protect)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemVirtualQuery(
        process: *mut c_void,
        address: *const c_void,
        out: *mut xmem::BasicInformation,
    ) -> i32 {
        xmem::XMemVirtualQuery(process, address, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemGetWorkingSetStatistics(which: u32, out: *mut WorkingSet) -> i32 {
        init_title_image();
        xmem::XMemGetWorkingSetStatistics(which, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> i32 {
        init_title_image();
        xmem::XMemTransferMemory(kind, size)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XGSuggestSwizzleMode(
        desc: *const xg::ResourceDesc,
        flags: u32,
        out: *mut u32,
    ) -> i32 {
        xg::XGSuggestSwizzleMode(desc, flags, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XGCreateTextureComputer(
        desc: *const xg::ResourceDesc,
        out: *mut *mut xg::Computer,
    ) -> i32 {
        xg::XGCreateTextureComputer(desc, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XDspConnectWithMaximumStreamLimit(
        base: *mut c_void,
        base_length: u32,
        ir_seconds: u32,
        max_streams: u32,
        max_channels: u32,
        out: *mut *mut dsp::Dsp,
    ) -> i32 {
        dsp::XDspConnectWithMaximumStreamLimit(
            base,
            base_length,
            ir_seconds,
            max_streams,
            max_channels,
            out,
        )
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XDspDisconnect(dsp: *mut dsp::Dsp) -> i32 {
        dsp::XDspDisconnect(dsp)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XDspActivate(
        dsp: *mut dsp::Dsp,
        desc: *const dsp::StreamDesc,
        status: *mut *const dsp::Status,
        stream: *mut *mut dsp::Stream,
    ) -> i32 {
        dsp::XDspActivate(dsp, desc, status, stream)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XDspDeactivate(stream: *mut dsp::Stream) -> i32 {
        dsp::XDspDeactivate(stream)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XDspSubmitCommand(
        stream: *mut dsp::Stream,
        command: *const dsp::Command,
        ticket: *mut u32,
    ) -> i32 {
        dsp::XDspSubmitCommand(stream, command, ticket)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XApuConnect(
        input: *const apu::ConnectInput,
        output: *mut apu::ConnectOutput,
        handle: *mut *mut apu::Connection,
    ) -> i32 {
        apu::XApuConnect(input, output, handle)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XApuDisconnect(handle: *mut apu::Connection) -> i32 {
        apu::XApuDisconnect(handle)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XApuEnqueueCommand(
        handle: *mut apu::Connection,
        command: *const apu::CommandId,
        total: *mut u32,
    ) -> i32 {
        apu::XApuEnqueueCommand(handle, command, total)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XApuDequeueResult(
        handle: *mut apu::Connection,
        out: *mut apu::ApuResult,
    ) -> i32 {
        apu::XApuDequeueResult(handle, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XApuGetResultCount(
        handle: *mut apu::Connection,
        total: *mut u32,
    ) -> u32 {
        apu::XApuGetResultCount(handle, total)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn AcpHalAllocateShapeContexts(contexts: *mut audio::ShapeContexts) -> i32 {
        audio::AcpHalAllocateShapeContexts(contexts)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn AcpHalReleaseShapeContexts() -> i32 {
        audio::AcpHalReleaseShapeContexts()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn AcpHalCreate(out: *mut *mut audio::Acp) -> i32 {
        audio::AcpHalCreate(out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuCreateHeap(cached: u32, non_cached: u32) -> i32 {
        audio::ApuCreateHeap(cached, non_cached)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuMapApuAddress(apu: u32) -> *mut c_void {
        audio::ApuMapApuAddress(apu)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuAlloc(
        out: *mut *mut c_void,
        apu: *mut u32,
        size: u32,
        align: u32,
        flags: u32,
    ) -> i32 {
        audio::ApuAlloc(out, apu, size, align, flags)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuFree(ptr: *mut c_void) {
        audio::ApuFree(ptr);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuMapVirtualAddress(ptr: *const c_void) -> u32 {
        audio::ApuMapVirtualAddress(ptr)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn ApuIsVirtualAddressValid(ptr: *const c_void, align: u32) -> bool {
        audio::ApuIsVirtualAddressValid(ptr, align)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn DStorageGetFactory(
        iid: *const xgameruntime::Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        dstorage::DStorageGetFactory(iid, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn GameInputCreate(out: *mut *const c_void) -> i32 {
        init_title_image();
        gameinput::GameInputCreate(out.cast())
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn DllCanUnloadNow() -> i32 {
        1 // S_FALSE: the runtime owns process-wide objects.
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn InitializeApiImpl(version: u64, build: u64) -> i32 {
        init_title_image();
        xgameruntime::InitializeApiImpl(version, build)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn InitializeApiImplEx(version: u64, build: u64, flags: u64) -> i32 {
        init_title_image();
        xgameruntime::InitializeApiImplEx(version, build, flags)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn InitializeApiImplEx2(
        version: u64,
        build: u64,
        flags: u64,
        options: *const c_void,
    ) -> i32 {
        init_title_image();
        xgameruntime::InitializeApiImplEx2(version, build, flags, options)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn QueryApiImpl(
        provider: *const xgameruntime::Guid,
        iid: *const xgameruntime::Guid,
        out: *mut *mut c_void,
    ) -> i32 {
        init_title_image();
        xgameruntime::QueryApiImpl(provider, iid, out)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn UninitializeApiImpl() {
        init_title_image();
        xgameruntime::UninitializeApiImpl();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn XErrorReport(hr: i32, message: *const c_char) {
        init_title_image();
        xgameruntime::XErrorReport(hr, message);
    }

    #[unsafe(export_name = "VictoryD3D12ConfigureVirtualMemoryX")]
    pub extern "system" fn D3D12ConfigureVirtualMemoryX(config: *mut device::VirtualMemory) -> i32 {
        device::configure_virtual_memory(config)
    }

    #[unsafe(export_name = "VictoryD3D12XboxCreateDevice")]
    pub extern "system" fn D3D12XboxCreateDevice(
        adapter: *const c_void,
        parameters: *const u32,
        iid: *const object::Iid,
        out: *mut *const object::Object,
    ) -> i32 {
        init_title_image();
        d3d12x::create_device(adapter, parameters, iid, out)
    }

    #[unsafe(export_name = "VictoryD3D12SerializeRootSignature")]
    pub extern "system" fn D3D12SerializeRootSignature(
        desc: *const root::Desc,
        version: u32,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> i32 {
        root::serialize(desc, version, blob, error)
    }

    #[unsafe(export_name = "VictoryD3D12SerializeVersionedRootSignature")]
    pub extern "system" fn D3D12SerializeVersionedRootSignature(
        desc: *const root::VersionedDesc,
        blob: *mut *mut c_void,
        error: *mut *mut c_void,
    ) -> i32 {
        root::serialize_versioned(desc, blob, error)
    }

    #[unsafe(export_name = "VictoryD3D12CreateRootSignatureDeserializer")]
    pub extern "system" fn D3D12CreateRootSignatureDeserializer(
        blob: *const u8,
        size: usize,
        iid: *const object::Iid,
        out: *mut *mut c_void,
    ) -> i32 {
        root::create_deserializer(blob, size, iid, out)
    }

    #[unsafe(export_name = "VictoryIID_ID3D12Device")]
    pub static IID_ID3D12_DEVICE: object::Iid = device::IID;

    #[unsafe(export_name = "VictoryGuardConnect")]
    pub extern "system" fn guard_connect(socket: usize, address: *const u8, len: i32) -> i32 {
        init_title_image();
        net::connect(socket, address, len)
    }

    #[unsafe(export_name = "VictoryGuardBind")]
    pub extern "system" fn guard_bind(socket: usize, address: *const u8, len: i32) -> i32 {
        init_title_image();
        net::bind(socket, address, len)
    }

    #[unsafe(export_name = "VictoryGuardSendTo")]
    pub extern "system" fn guard_send_to(
        socket: usize,
        buffer: *const u8,
        len: i32,
        flags: i32,
        to: *const u8,
        to_len: i32,
    ) -> i32 {
        init_title_image();
        net::sendto(socket, buffer, len, flags, to, to_len)
    }

    #[unsafe(export_name = "VictoryGuardGetAddrInfo")]
    pub extern "system" fn guard_get_addr_info(
        node: *const c_char,
        service: *const c_char,
        hints: *const c_void,
        result: *mut *mut c_void,
    ) -> i32 {
        init_title_image();
        net::getaddrinfo(node, service, hints, result)
    }

    #[unsafe(export_name = "VictoryGuardGetHostByName")]
    pub extern "system" fn guard_get_host_by_name(name: *const c_char) -> *mut c_void {
        init_title_image();
        net::gethostbyname(name)
    }

    #[unsafe(export_name = "VictoryGuardWinHttpConnect")]
    pub extern "system" fn guard_win_http_connect(
        session: *mut c_void,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> *mut c_void {
        init_title_image();
        net::WinHttpConnect(session, server, port, reserved)
    }

    #[unsafe(export_name = "VictoryGuardWinHttpOpen")]
    pub extern "system" fn guard_win_http_open(
        agent: *const u16,
        access: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> *mut c_void {
        init_title_image();
        net::WinHttpOpen(agent, access, proxy, bypass, flags)
    }

    #[unsafe(export_name = "VictoryGuardWinHttpSetOption")]
    pub extern "system" fn guard_win_http_set_option(
        handle: *mut c_void,
        option: u32,
        buffer: *const c_void,
        len: u32,
    ) -> i32 {
        init_title_image();
        net::WinHttpSetOption(handle, option, buffer, len)
    }

    #[unsafe(export_name = "VictoryWerRegisterFile")]
    pub extern "system" fn local_wer_register_file(path: *const u16, kind: u32, flags: u32) -> i32 {
        init_title_image();
        tracing::info!("title crash attachment registration ignored");
        let _ = (path, kind, flags);
        0 // S_OK: no attachment is handed to Windows Error Reporting.
    }
}
