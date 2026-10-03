use std::sync::LazyLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub instructions: String,
}

impl Skill {
    pub fn prompt_block(&self) -> String {
        format!(
            "<active-skill id=\"{}\" name=\"{}\">\n{}\n</active-skill>\n\n\
             The block above is a specialization the user selected for this task. It \
             refines how you approach the work -- it does not change who you are, what \
             tools you have, or what the user asked for. Follow it alongside the rules \
             above, and prefer it wherever the two differ on approach or emphasis.",
            self.id,
            self.name,
            self.instructions.trim()
        )
    }
}

const RAW: &[&str] = &[
    include_str!("../skills/frontend-design.md"),
    include_str!("../skills/code-review.md"),
    include_str!("../skills/test-writing.md"),
    include_str!("../skills/debugging-root-cause.md"),
    include_str!("../skills/documentation.md"),
];

static SKILLS: LazyLock<Vec<Skill>> = LazyLock::new(|| {
    RAW.iter()
        .map(|raw| parse(raw).expect("built-in skill file is malformed"))
        .collect()
});

/// Every built-in skill, in catalog order.
pub fn all() -> &'static [Skill] {
    &SKILLS
}

pub fn find(id: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|s| s.id == id)
}

fn parse(raw: &str) -> Result<Skill, String> {
    // `include_str!` embeds the file's literal on-disk bytes, and a Windows
    // checkout with `core.autocrlf` gives those `\r\n` line endings --
    // normalize once so the rest of this hand-rolled parser can assume `\n`
    // the same way it would on any other platform.
    let raw = raw.replace("\r\n", "\n");
    let body = raw
        .strip_prefix("---\n")
        .ok_or("skill file must start with a --- frontmatter fence")?;
    let (front, instructions) = body
        .split_once("\n---\n")
        .ok_or("skill frontmatter is not closed by a --- line")?;

    let mut id = None;
    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| format!("frontmatter line is not `key: value`: {line}"))?;
        let value = value.trim().to_string();
        match key.trim() {
            "id" => id = Some(value),
            "name" => name = Some(value),
            "description" => description = Some(value),
            other => return Err(format!("unknown frontmatter key: {other}")),
        }
    }

    let skill = Skill {
        id: id.ok_or("skill frontmatter is missing `id`")?,
        name: name.ok_or("skill frontmatter is missing `name`")?,
        description: description.ok_or("skill frontmatter is missing `description`")?,
        instructions: instructions.trim().to_string(),
    };
    if skill.id.is_empty() || skill.name.is_empty() || skill.description.is_empty() {
        return Err(format!("skill `{}` has an empty header field", skill.id));
    }
    if skill.instructions.is_empty() {
        return Err(format!("skill `{}` has no instructions", skill.id));
    }
    Ok(skill)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `all()` panics lazily on a malformed file, so nothing would catch a bad
    // edit until a user selected that skill. This is what catches it.
    #[test]
    fn every_built_in_skill_parses() {
        assert_eq!(all().len(), RAW.len());
    }

    #[test]
    fn ids_are_unique_and_findable() {
        let mut ids: Vec<&str> = all().iter().map(|s| s.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate skill id");
        for skill in all() {
            assert_eq!(find(&skill.id), Some(skill));
        }
    }

    #[test]
    fn an_unknown_id_is_none_not_a_panic() {
        assert_eq!(find("no-such-skill"), None);
        assert_eq!(find(""), None);
    }

    #[test]
    fn the_expected_five_skills_ship() {
        let ids: Vec<&str> = all().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "frontend-design",
                "code-review",
                "test-writing",
                "debugging-root-cause",
                "documentation"
            ]
        );
    }

    #[test]
    fn the_prompt_block_fences_the_instructions() {
        let skill = find("code-review").expect("code-review ships");
        let block = skill.prompt_block();
        assert!(block.starts_with("<active-skill id=\"code-review\""));
        assert!(block.contains("</active-skill>"));
        assert!(block.contains(skill.instructions.trim()));
    }

    #[test]
    fn frontmatter_must_be_present_and_closed() {
        assert!(parse("no frontmatter here").is_err());
        assert!(parse("---\nid: x\nname: X\ndescription: d\nbody, unfenced").is_err());
    }

    #[test]
    fn a_description_may_contain_a_colon() {
        let skill = parse("---\nid: x\nname: X\ndescription: does a thing: carefully\n---\nbody")
            .expect("colons in a value are fine");
        assert_eq!(skill.description, "does a thing: carefully");
    }

    #[test]
    fn a_missing_field_or_empty_body_is_an_error() {
        assert!(parse("---\nid: x\nname: X\n---\nbody").is_err());
        assert!(parse("---\nid: x\nname: X\ndescription: d\n---\n   \n").is_err());
    }
}
