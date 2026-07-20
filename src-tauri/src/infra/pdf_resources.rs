use std::io::Read;

use flate2::read::ZlibDecoder;
use lopdf::{Document, Object, Stream};

use crate::domain::error::AppError;

pub(crate) const DEFAULT_PDF_PAGES_PER_ITEM: usize = 100;
pub(crate) const DEFAULT_PDF_OBJECTS_PER_ITEM: usize = 20_000;
pub(crate) const DEFAULT_PDF_DECODED_STREAM_BYTES_PER_ITEM: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PdfResourceLimits {
    max_pages_per_item: usize,
    max_pages_total: usize,
    max_objects_per_item: usize,
    max_objects_total: usize,
    max_decoded_stream_bytes_per_item: u64,
    max_decoded_stream_bytes_total: u64,
}

impl PdfResourceLimits {
    pub(crate) const fn new(
        max_pages_per_item: usize,
        max_pages_total: usize,
        max_objects_per_item: usize,
        max_objects_total: usize,
        max_decoded_stream_bytes_per_item: u64,
        max_decoded_stream_bytes_total: u64,
    ) -> Self {
        Self {
            max_pages_per_item,
            max_pages_total,
            max_objects_per_item,
            max_objects_total,
            max_decoded_stream_bytes_per_item,
            max_decoded_stream_bytes_total,
        }
    }

    pub(crate) const fn single_item(
        max_pages: usize,
        max_objects: usize,
        max_decoded_stream_bytes: u64,
    ) -> Self {
        Self::new(
            max_pages,
            max_pages,
            max_objects,
            max_objects,
            max_decoded_stream_bytes,
            max_decoded_stream_bytes,
        )
    }
}

#[derive(Debug, Default)]
pub(crate) struct PdfResourceUsage {
    pages: usize,
    objects: usize,
    decoded_stream_bytes: u64,
}

pub(crate) fn validate_pdf_resources(
    document: &Document,
    usage: &mut PdfResourceUsage,
    limits: PdfResourceLimits,
) -> Result<(), AppError> {
    let pages = document.get_pages().len();
    if pages > limits.max_pages_per_item {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 页数超过导出限制",
        ));
    }
    let total_pages = usage
        .pages
        .checked_add(pages)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 页数超过导出限制"))?;
    if total_pages > limits.max_pages_total {
        return Err(validation_error("normalizedPdf", "PDF 总页数超过导出限制"));
    }

    let objects = document.objects.len();
    if objects > limits.max_objects_per_item {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 对象数超过导出限制",
        ));
    }
    let total_objects = usage
        .objects
        .checked_add(objects)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 对象数超过导出限制"))?;
    if total_objects > limits.max_objects_total {
        return Err(validation_error(
            "normalizedPdf",
            "PDF 总对象数超过导出限制",
        ));
    }

    let mut decoded_stream_bytes = 0_u64;
    for object in document.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        let item_remaining = limits
            .max_decoded_stream_bytes_per_item
            .saturating_sub(decoded_stream_bytes);
        let package_remaining = remaining_decoded_stream_budget(
            limits.max_decoded_stream_bytes_total,
            usage.decoded_stream_bytes,
            decoded_stream_bytes,
        )?;
        let decoded_len = bounded_stream_size(stream, item_remaining.min(package_remaining))?;
        decoded_stream_bytes = decoded_stream_bytes
            .checked_add(decoded_len)
            .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
        if decoded_stream_bytes > limits.max_decoded_stream_bytes_per_item {
            return Err(validation_error(
                "normalizedPdf",
                "单张票据 PDF 解码数据超过导出限制",
            ));
        }
    }
    let total_decoded_stream_bytes = usage
        .decoded_stream_bytes
        .checked_add(decoded_stream_bytes)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
    if total_decoded_stream_bytes > limits.max_decoded_stream_bytes_total {
        return Err(validation_error(
            "normalizedPdf",
            "PDF 解码数据总量超过导出限制",
        ));
    }

    usage.pages = total_pages;
    usage.objects = total_objects;
    usage.decoded_stream_bytes = total_decoded_stream_bytes;
    Ok(())
}

fn remaining_decoded_stream_budget(
    limit: u64,
    committed: u64,
    current: u64,
) -> Result<u64, AppError> {
    let used = committed
        .checked_add(current)
        .ok_or_else(|| validation_error("normalizedPdf", "PDF 数据流超过导出限制"))?;
    Ok(limit.saturating_sub(used))
}

fn bounded_stream_size(stream: &Stream, limit: u64) -> Result<u64, AppError> {
    let filters = match stream.filters() {
        Ok(filters) => filters,
        Err(_) if stream.dict.get(b"Filter").is_err() => {
            return bounded_opaque_size(stream.content.len(), limit);
        }
        Err(_) => return Err(unsupported_stream_filter()),
    };
    if filters.len() != 1 {
        return Err(unsupported_stream_filter());
    }
    match filters[0] {
        b"FlateDecode" | b"Fl" => bounded_flate_size(&stream.content, limit),
        // These codecs remain opaque in lopdf's load/renumber/save path. The validator never
        // calls lopdf's content decoders for them, so their stored bytes are the allocation bound.
        b"DCTDecode" | b"DCT" | b"JPXDecode" | b"CCITTFaxDecode" | b"CCF" | b"JBIG2Decode"
        | b"Crypt" => bounded_opaque_size(stream.content.len(), limit),
        // lopdf's LZW and ASCII85 helpers allocate complete output buffers. Fail closed until a
        // bounded streaming implementation is available for these filters.
        _ => Err(unsupported_stream_filter()),
    }
}

fn bounded_flate_size(content: &[u8], limit: u64) -> Result<u64, AppError> {
    let decoder = ZlibDecoder::new(content);
    let mut bounded = decoder.take(limit.saturating_add(1));
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = bounded
            .read(&mut buffer)
            .map_err(|_| validation_error("normalizedPdf", "PDF 数据流无法解码"))?;
        if read == 0 {
            return Ok(total);
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(validation_error(
                "normalizedPdf",
                "单张票据 PDF 解码数据超过导出限制",
            ));
        }
    }
}

fn bounded_opaque_size(size: usize, limit: u64) -> Result<u64, AppError> {
    let size = size as u64;
    if size > limit {
        return Err(validation_error(
            "normalizedPdf",
            "单张票据 PDF 解码数据超过导出限制",
        ));
    }
    Ok(size)
}

fn unsupported_stream_filter() -> AppError {
    validation_error("normalizedPdf", "PDF 数据流过滤器不受安全导出支持")
}

fn validation_error(field: &str, message: &str) -> AppError {
    AppError::validation(field, message)
}

#[cfg(test)]
mod tests {
    use lopdf::{Document, Object, Stream, dictionary};

    use super::{
        PdfResourceLimits, PdfResourceUsage, remaining_decoded_stream_budget,
        validate_pdf_resources,
    };

    #[test]
    fn pdf_page_budget_is_enforced() {
        let document = pdf_with_pages(2);
        let mut usage = PdfResourceUsage::default();

        let error = validate_pdf_resources(&document, &mut usage, pdf_limits(1)).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_object_budget_is_enforced() {
        let mut document = pdf_with_pages(1);
        document.add_object(Object::Null);
        let mut usage = PdfResourceUsage::default();
        let mut limits = pdf_limits(10);
        limits.max_objects_per_item = document.objects.len() - 1;

        let error = validate_pdf_resources(&document, &mut usage, limits).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_decoded_stream_budget_is_enforced() {
        let mut document = pdf_with_pages(1);
        document.add_object(Stream::new(dictionary! {}, vec![0_u8; 17]));
        let mut usage = PdfResourceUsage::default();
        let mut limits = pdf_limits(10);
        limits.max_decoded_stream_bytes_per_item = 16;

        let error = validate_pdf_resources(&document, &mut usage, limits).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    #[test]
    fn pdf_decoded_stream_budget_overflow_is_rejected() {
        let error = remaining_decoded_stream_budget(u64::MAX, u64::MAX, 1).unwrap_err();

        assert!(
            matches!(error, crate::domain::error::AppError::Validation { field, .. } if field == "normalizedPdf")
        );
    }

    fn pdf_limits(max_pages_per_item: usize) -> PdfResourceLimits {
        PdfResourceLimits {
            max_pages_per_item,
            max_pages_total: usize::MAX,
            max_objects_per_item: usize::MAX,
            max_objects_total: usize::MAX,
            max_decoded_stream_bytes_per_item: u64::MAX,
            max_decoded_stream_bytes_total: u64::MAX,
        }
    }

    fn pdf_with_pages(page_count: usize) -> Document {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let page_ids = (0..page_count)
            .map(|_| {
                document.add_object(dictionary! {
                    "Type" => "Page",
                    "Parent" => pages_id,
                    "MediaBox" => vec![0.into(), 0.into(), 100.into(), 150.into()],
                })
            })
            .collect::<Vec<_>>();
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids
                    .iter()
                    .copied()
                    .map(Object::Reference)
                    .collect::<Vec<_>>(),
                "Count" => page_count as i64,
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        document.trailer.set("Root", catalog_id);
        document
    }
}
