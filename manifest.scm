(use-modules (gnu packages))

(specifications->manifest
 (list "rust"
       "pkg-config"
       "clang"
       "libxkbcommon"
       "wayland"
       "wayland-protocols"
       "vulkan-loader"))
