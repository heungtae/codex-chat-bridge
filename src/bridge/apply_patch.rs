pub(crate) fn normalize_apply_patch_input(raw: &str) -> String {
    let trimmed = raw.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let unfenced = strip_code_fence(trimmed);
    let extracted = extract_patch_block(unfenced).unwrap_or(unfenced);
    extracted
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
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
