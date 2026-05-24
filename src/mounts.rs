use std::{fs, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub target: PathBuf,
    pub fs_type: String,
}

pub fn discover_mounts() -> Vec<Mount> {
    fs::read_to_string("/proc/self/mountinfo")
        .map(|data| parse_mountinfo(&data))
        .unwrap_or_default()
}

pub fn parse_mountinfo(input: &str) -> Vec<Mount> {
    let mut mounts = input
        .lines()
        .filter_map(parse_mount_line)
        .filter(|mount| !is_pseudo_fs(&mount.fs_type))
        .collect::<Vec<_>>();

    mounts.sort_by(|a, b| a.target.cmp(&b.target));
    mounts.dedup_by(|a, b| a.target == b.target);
    mounts
}

fn parse_mount_line(line: &str) -> Option<Mount> {
    let (left, right) = line.split_once(" - ")?;
    let mut left_fields = left.split_whitespace();
    let target = left_fields.nth(4)?;
    let mut right_fields = right.split_whitespace();
    let fs_type = right_fields.next()?.to_string();
    let source = right_fields.next().unwrap_or("").to_string();

    Some(Mount {
        source,
        target: unescape_mount_path(target).into(),
        fs_type,
    })
}

fn is_pseudo_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "autofs"
            | "binfmt_misc"
    )
}

fn unescape_mount_path(path: &str) -> String {
    path.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_mounts_and_omits_pseudo_filesystems() {
        let input = "\
25 1 0:21 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw
26 1 8:2 / / rw,relatime - ext4 /dev/sda2 rw
27 1 8:3 / /home/trev/My\\040Disk rw,relatime - btrfs /dev/sda3 rw
";

        let mounts = parse_mountinfo(input);

        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].target, PathBuf::from("/"));
        assert_eq!(mounts[1].target, PathBuf::from("/home/trev/My Disk"));
    }
}
