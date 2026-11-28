use super::*;
const STEP: usize = 50;
pub(super) fn paging(v: &mut Value, sheet: usize, page: usize, more: bool, names: Vec<String>) {
    v["paging"] = json!({"sheet":sheet,"page":page,"more":more,"names":names});
}

// 新文件面板只传选中的表/50 行、50 个正文块或一张幻灯片。旧接口仍保留原有返回结构。
pub(crate) fn file_window(
    f: &mut std::fs::File,
    size: u64,
    ext: &str,
    sheet: usize,
    page: usize,
) -> PreviewResult<Value> {
    if size > 64 * 1024 * 1024 || page >= 2000 || sheet >= MAX_SHEETS {
        return Err("文件输入或请求范围超过办公文档预览预算");
    }
    let reader = BufReader::new(f);
    if matches!(ext, "xlsx" | "xlsm" | "xltx") {
        return spreadsheet_from(reader, Some((sheet, page)));
    }
    let mut zip = checked_zip(reader)?;
    let mut budget = Budget::default();
    if ext == "pptx" {
        let main = xml_entry(&mut zip, "ppt/presentation.xml")?;
        let doc = xml(&main)?;
        let rel = xml_entry(&mut zip, "ppt/_rels/presentation.xml.rels")?;
        let rel = xml(&rel)?;
        let ids: Vec<_> = doc
            .descendants()
            .filter(|n| element(*n, "sldId"))
            .filter_map(|n| {
                n.attributes()
                    .find(|a| a.name() == "id" && a.namespace().is_some())
                    .map(|a| a.value())
            })
            .take(MAX_SECTIONS)
            .collect();
        let id = ids.get(page).ok_or("幻灯片页码不存在")?;
        let item = rel
            .descendants()
            .find(|n| {
                element(*n, "Relationship")
                    && n.attribute("Id") == Some(*id)
                    && n.attribute("TargetMode") != Some("External")
                    && n.attribute("Type").is_some_and(|s| s.ends_with("/slide"))
            })
            .ok_or("幻灯片关系无效")?;
        let path = package_part(
            "ppt/presentation.xml",
            item.attribute("Target").ok_or("幻灯片缺少路径")?,
        )?;
        if !path.starts_with("ppt/slides/") {
            return Err("幻灯片内容路径无效");
        }
        let source = xml_entry(&mut zip, &path)?;
        let doc = xml(&source)?;
        let mut blocks = Vec::new();
        collect_blocks(doc.root_element(), &mut blocks, &mut budget, 0);
        let mut value = document_value(
            vec![Section {
                title: format!("第 {} 页", page + 1),
                blocks,
            }],
            budget.truncated,
        );
        paging(&mut value, 0, page, page + 1 < ids.len(), vec![]);
        return Ok(value);
    }
    let source = xml_entry(
        &mut zip,
        if ext == "docx" {
            "word/document.xml"
        } else {
            "content.xml"
        },
    )?;
    let doc = xml(&source)?;
    if ext == "ods" {
        let tables: Vec<_> = doc
            .descendants()
            .filter(|n| element(*n, "table"))
            .take(MAX_SHEETS)
            .collect();
        let table = tables.get(sheet).ok_or("工作表不存在")?;
        let names: Vec<_> = tables
            .iter()
            .enumerate()
            .map(|(i, n)| {
                n.attributes()
                    .find(|a| a.name() == "name")
                    .map(|a| a.value().chars().take(256).collect())
                    .unwrap_or_else(|| format!("工作表 {}", i + 1))
            })
            .collect();
        let mut builder = SheetBuilder::default();
        let mut ordinal = 0usize;
        let mut physical = 0usize;
        let mut more = false;
        for row in table.children().filter(|n| element(*n, "table-row")) {
            let repeat_rows = repeat(row, "number-rows-repeated");
            let start = physical;
            physical = physical.saturating_add(repeat_rows);
            let mut values = Vec::new();
            let mut column = 0usize;
            for cell in row
                .children()
                .filter(|n| element(*n, "table-cell") || element(*n, "covered-table-cell"))
            {
                if column >= MAX_COLUMNS {
                    break;
                }
                let mut text = cell
                    .descendants()
                    .filter(|n| element(*n, "p"))
                    .map(paragraph_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.is_empty() {
                    text = cell
                        .attributes()
                        .find(|a| matches!(a.name(), "string-value" | "value" | "date-value"))
                        .map(|a| a.value().to_owned())
                        .unwrap_or_default();
                }
                let copies = repeat(cell, "number-columns-repeated").min(MAX_COLUMNS - column);
                if !text.is_empty() {
                    values.push((column, copies, text))
                }
                column += copies;
            }
            if values.is_empty() {
                continue;
            }
            let prev = ordinal;
            ordinal = ordinal.saturating_add(repeat_rows);
            let from = prev.max(page * STEP);
            let to = ordinal.min((page + 1) * STEP);
            for i in from..to {
                let number = start
                    .saturating_add(i - prev)
                    .saturating_add(1)
                    .min(u32::MAX as usize) as u32;
                for (col, copies, text) in &values {
                    for c in *col..*col + *copies {
                        builder.insert(number, c, text, &mut budget);
                    }
                }
            }
            if ordinal > (page + 1) * STEP {
                more = true;
                break;
            }
        }
        let mut value =
            spreadsheet_value(vec![builder.finish(names[sheet].clone())], budget.truncated);
        paging(&mut value, sheet, page, more, names);
        return Ok(value);
    }
    let body = doc
        .descendants()
        .find(|n| element(*n, if ext == "docx" { "body" } else { "text" }))
        .ok_or("文档缺少正文")?;
    let mut blocks = Vec::new();
    // 只遍历正文块，不将表格里的段落重复当成独立段落。
    let mut groups = body
        .descendants()
        .filter(|n| {
            element(*n, "p") || element(*n, "h") || element(*n, "tbl") || element(*n, "table")
        })
        .filter(|n| {
            !n.ancestors()
                .skip(1)
                .take_while(|a| *a != body)
                .any(|a| element(a, "tbl") || element(a, "table") || element(a, "p"))
        })
        .skip(page * STEP);
    for n in groups.by_ref().take(STEP) {
        if element(n, "p") || element(n, "h") {
            let text = budget.text(&paragraph_text(n));
            let heading = element(n, "h")
                || n.descendants().any(|v| {
                    element(v, "pStyle")
                        && v.attributes().any(|a| {
                            a.name() == "val"
                                && a.value().to_ascii_lowercase().starts_with("heading")
                        })
                });
            blocks.push(if heading {
                Block::Heading { text }
            } else {
                Block::Paragraph { text }
            });
        } else {
            let mut rows = Vec::new();
            for r in n
                .children()
                .filter(|v| element(*v, "tr") || element(*v, "table-row"))
                .take(MAX_ROWS)
            {
                let mut cells = Vec::new();
                for c in r
                    .children()
                    .filter(|v| element(*v, "tc") || element(*v, "table-cell"))
                    .take(MAX_COLUMNS)
                {
                    if budget.cells >= MAX_CELLS || budget.bytes >= MAX_TEXT {
                        budget.truncated = true;
                        break;
                    }
                    budget.cells += 1;
                    cells.push(budget.text(&paragraph_text(c)));
                }
                rows.push(cells);
                if budget.truncated {
                    break;
                }
            }
            blocks.push(Block::Table { rows });
        }
    }
    let mut value = document_value(
        vec![Section {
            title: String::new(),
            blocks,
        }],
        budget.truncated,
    );
    paging(&mut value, 0, page, groups.next().is_some(), vec![]);
    Ok(value)
}
