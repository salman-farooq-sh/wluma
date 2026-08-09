use std::os::fd::OwnedFd;

pub struct Object {
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub format: u32,
    pub layout: Option<(u64, u32, u32)>,
    fds: Vec<Option<OwnedFd>>,
    pub sizes: Vec<u32>,
}

impl Object {
    pub fn new(width: u32, height: u32, num_objects: u32, format: u32) -> Self {
        Self {
            width,
            height,
            num_objects,
            format,
            layout: None,
            fds: std::iter::repeat_with(|| None)
                .take(num_objects as usize)
                .collect(),
            sizes: vec![0; num_objects as usize],
        }
    }

    pub fn set_object(&mut self, index: u32, fd: OwnedFd, size: u32) {
        self.fds[index as usize] = Some(fd);
        self.sizes[index as usize] = size;
    }

    pub fn fd(&self, index: usize) -> &OwnedFd {
        self.fds[index]
            .as_ref()
            .expect("DMA-BUF object was not provided")
    }
}
