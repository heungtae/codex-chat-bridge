#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApplyPatchNormalization {
    pub normalized: String,
    pub repairs: Vec<String>,
}

pub(crate) fn normalize_apply_patch_input_with_repairs(raw: &str) -> ApplyPatchNormalization {
    let trimmed = raw.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return ApplyPatchNormalization {
            normalized: String::new(),
            repairs: Vec::new(),
        };
    }

    let unfenced = strip_code_fence(trimmed);
    let extracted = extract_patch_block(unfenced).unwrap_or(unfenced);
    let normalized = extracted
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    let (normalized, repairs) = repair_add_file_lines(normalized.trim());
    ApplyPatchNormalization {
        normalized,
        repairs,
    }
}

fn strip_code_fence(input: &str) -> &str {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() < 2 {
        return input;
    }

    let Some(first) = lines.first().map(|line| line.trim()) else {
        return input;
    };
    let Some(last) = lines.last().map(|line| line.trim()) else {
        return input;
    };

    if !first.starts_with("```") || last != "```" {
        return input;
    }

    let start = input.find('\n').map(|idx| idx + 1).unwrap_or(input.len());
    let end = input.rfind("\n```").unwrap_or(input.len());
    input[start..end].trim()
}

fn extract_patch_block(input: &str) -> Option<&str> {
    let start = input.find("*** Begin Patch")?;
    let after_start = &input[start..];
    let end_marker = "*** End Patch";
    let end_offset = after_start.find(end_marker)?;
    let end = start + end_offset + end_marker.len();
    Some(input[start..end].trim())
}

fn repair_add_file_lines(input: &str) -> (String, Vec<String>) {
    let mut output = Vec::new();
    let mut repairs = Vec::new();
    let mut in_add_file = false;
    let mut current_file: Option<&str> = None;
    let mut add_file_line_no = 0usize;

    for line in input.lines() {
        if in_add_file && is_patch_header(line) {
            in_add_file = false;
            current_file = None;
            add_file_line_no = 0;
        }

        if in_add_file {
            add_file_line_no += 1;
            if line.strip_prefix('+').is_none() {
                let file = current_file.unwrap_or("<unknown>");
                let repair_kind = if line.is_empty() {
                    "added missing '+' prefix for blank add-file line"
                } else {
                    "added missing '+' prefix for add-file content line"
                };
                repairs.push(format!("{repair_kind} ({file}:{add_file_line_no})"));
                output.push(format!("+{line}"));
            } else {
                output.push(line.to_string());
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            in_add_file = true;
            current_file = Some(path);
            add_file_line_no = 0;
        }

        output.push(line.to_string());
    }

    (output.join("\n"), repairs)
}

fn is_patch_header(line: &str) -> bool {
    matches!(
        line,
        "*** End Patch" | "@@" | "*** Add File: " | "*** Delete File: " | "*** Update File: "
    ) || line.starts_with("@@ ")
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}
