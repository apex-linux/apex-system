# bootupd 0.2.35 golden

`grub2-static/` is `/usr/lib/bootupd/grub2-static` as bootupd v0.2.35 installs it
(`src/grub2/*.cfg` and `src/grub2/configs.d/*`, Apache-2.0), plus the Apex drop-ins
`07_apex-menu.cfg` and `08_apex-bootcount.cfg` as they were when the golden was made.
`configs.d/README.md` stays: bootupd skips names that do not end in `.cfg`.

`grub.cfg` is what bootupd's own `grubconfigs::install(target, Some(src), None, false)`
wrote for that tree, run from a test added to a v0.2.35 checkout. Regenerate it the same
way, in a new `bootupd-<version>` directory, whenever the image moves to another bootupd.
