use crate::ProjectId;

pub const LIMIT: usize = 4;
pub static ENABLED: bool = true;
pub type ResultAlias = Result<(), ()>;

pub struct ProjectService;
pub enum State {
    Ready,
    Failed,
}

pub trait Runner {
    fn run(&self);
}

impl ProjectService {
    pub fn create_worktree(&self) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn creates_worktree() {}
}
