pub(crate) fn normalize_apply_patch_input(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let unfenced = strip_code_fence(trimmed);
    let extracted = extract_patch_block(unfenced).unwrap_or(unfenced);
    let normalized = extracted
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    repair_add_file_lines(normalized.trim())
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

fn repair_add_file_lines(input: &str) -> String {
    let mut output = Vec::new();
    let mut in_add_file = false;

    for line in input.lines() {
        if in_add_file && is_patch_header(line) {
            in_add_file = false;
        }

        if in_add_file {
            if let Some(stripped) = line.strip_prefix('+') {
                output.push(format!("+{stripped}"));
            } else {
                output.push(format!("+{line}"));
            }
            continue;
        }

        if line.starts_with("*** Add File: ") {
            in_add_file = true;
        }

        output.push(line.to_string());
    }

    output.join("\n")
}

fn is_patch_header(line: &str) -> bool {
    matches!(
        line,
        "*** End Patch"
            | "@@"
            | "*** Add File: "
            | "*** Delete File: "
            | "*** Update File: "
    ) || line.starts_with("@@ ")
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}
