use super::*;

// PDF 原件只在 Harness 所在电脑解码；远程得到一张限尺寸页图，不返回扫描件或文档原件。
#[cfg(windows)]
pub(super) fn page(f: &mut File, size: u64, index: usize) -> Result<Value, String> {
    use windows::{
        Data::Pdf::{PdfDocument, PdfPageRenderOptions},
        Graphics::Imaging::BitmapEncoder,
        Storage::Streams::{DataReader, DataWriter, InMemoryRandomAccessStream},
        Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    };
    if size > 32 * 1024 * 1024 {
        return Err("PDF 超过本机分页解码的 32 MiB 输入预算，未传输原件".into());
    }
    f.rewind().map_err(|e| e.to_string())?;
    let mut b = vec![0; size as usize];
    f.read_exact(&mut b).map_err(|e| e.to_string())?;
    struct Com;
    impl Drop for Com {
        fn drop(&mut self) {
            unsafe { RoUninitialize() }
        }
    }
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|_| "本机 PDF 阅读组件不可用")?;
    let _com = Com;
    let work = || -> windows::core::Result<Value> {
        let input = InMemoryRandomAccessStream::new()?;
        let writer = DataWriter::CreateDataWriter(&input.GetOutputStreamAt(0)?)?;
        writer.WriteBytes(&b)?;
        writer.StoreAsync()?.get()?;
        writer.DetachStream()?;
        drop(b);
        let doc = PdfDocument::LoadFromStreamAsync(&input)?.get()?;
        let count = doc.PageCount()?;
        if index >= count as usize {
            return Ok(unsupported("PDF 页码无效"));
        }
        let p = doc.GetPage(index as u32)?;
        let s = p.Size()?;
        if !s.Width.is_finite() || !s.Height.is_finite() || s.Width <= 0. || s.Height <= 0. {
            return Ok(unsupported("PDF 页面尺寸无效"));
        }
        let scale = 1400.0 / s.Width.max(s.Height);
        let opts = PdfPageRenderOptions::new()?;
        opts.SetDestinationWidth((s.Width * scale).round().clamp(1., 1400.) as u32)?;
        opts.SetDestinationHeight((s.Height * scale).round().clamp(1., 1400.) as u32)?;
        opts.SetBitmapEncoderId(BitmapEncoder::PngEncoderId()?)?;
        let out = InMemoryRandomAccessStream::new()?;
        p.RenderWithOptionsToStreamAsync(&out, &opts)?.get()?;
        p.Close()?;
        let n = out.Size()?;
        if n == 0 || n > 8 * 1024 * 1024 {
            return Ok(unsupported("PDF 页图超过安全预算"));
        }
        let reader = DataReader::CreateDataReader(&out.GetInputStreamAt(0)?)?;
        if reader.LoadAsync(n as u32)?.get()? != n as u32 {
            return Ok(unsupported("PDF 页图未完整生成"));
        }
        let mut image = vec![0; n as usize];
        reader.ReadBytes(&mut image)?;
        Ok(
            json!({"kind":"pdfPage","page":index,"pages":count,"textAvailable":size<=MAX_TEXT_BYTES as u64,
            "dataUrl":format!("data:image/png;base64,{}",STANDARD.encode(image))}),
        )
    };
    work().map_err(|e| {
        if e.code().0 as u32 == 0x8007052b {
            "PDF 已加密，请先解密".into()
        } else {
            format!("PDF 页面无法解析或绘制（{}）", e.code())
        }
    })
}
#[cfg(not(windows))]
pub(super) fn page(_: &mut File, _: u64, _: usize) -> Result<Value, String> {
    Err("当前宿主机不支持 PDF 原页绘制".into())
}
