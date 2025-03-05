cfgenius::define! {
    pub darwin = cfg(target_vendor="apple");
    pub ios_family = all(macro(darwin), cfg(target_os="ios"));
    pub have_machine_context = any(
        macro(darwin),
        cfg(target_os="fuchsia"),
        all(
            cfg(any(
                target_os="freebsd",
                target_os="haiku",
                target_os="netbsd",
                target_os="openbsd",
                target_os="linux",
                target_os="hurd"
            )),
            cfg(
                any(
                    target_arch="x86_64",
                    target_arch="arm",
                    target_arch="aarch64",
                    target_arch="riscv64", 
                )
            )
        ));

}

