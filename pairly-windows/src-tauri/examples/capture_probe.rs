//! Which ways of recording the screen work on this PC (desktop duplication, its newer form, or
//! plain GDI copies), for diagnosing screen sharing.

#[cfg(windows)]
fn main() {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    };
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::core::Interface;

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("factory");
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let desc = adapter.GetDesc1().expect("desc");
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            println!("adapter {a}: {name}");
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let od = output.GetDesc().expect("output desc");
                println!(
                    "  output {o}: {:?} attached={}",
                    od.DesktopCoordinates,
                    od.AttachedToDesktop.as_bool()
                );
                let mut device = None;
                let made = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                );
                println!("    device: {made:?}");
                if let Some(device) = device {
                    let o1: IDXGIOutput1 = output.cast().expect("output1");
                    println!(
                        "    DuplicateOutput: {:?}",
                        o1.DuplicateOutput(&device).map(|_| "ok")
                    );
                    if let Ok(o5) = output.cast::<IDXGIOutput5>() {
                        println!(
                            "    DuplicateOutput1: {:?}",
                            o5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                                .map(|_| "ok")
                        );
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
}

#[cfg(not(windows))]
fn main() {}
