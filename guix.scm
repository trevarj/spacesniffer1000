(use-modules (guix build-system cargo)
             (guix gexp)
             (guix import crate)
             ((guix licenses) #:prefix license:)
             (guix packages)
             (guix profiles)
             (guix utils)
             (gnu packages freedesktop)
             (gnu packages llvm)
             (gnu packages pkg-config)
             (gnu packages rust)
             (gnu packages vulkan)
             (gnu packages xdisorg))

(define %source-root (dirname (current-filename)))

(define %spacesniffer1000-lockfile
  (string-append %source-root "/Cargo.lock"))

(define (project-source? file stat)
  (and (not (string-contains file "/target/"))
       (not (string-suffix? "/target" file))
       (not (string-contains file "/.git/"))
       (not (string-suffix? "/.git" file))
       (not (string-contains file "/.codex/"))
       (not (string-suffix? "/.codex" file))
       (not (string-contains file "/.agents/"))
       (not (string-suffix? "/.agents" file))))

(define-public spacesniffer1000
  (package
    (name "spacesniffer1000")
    (version "0.1.0")
    (source (local-file %source-root
                        #:recursive? #t
                        #:select? project-source?))
    (build-system cargo-build-system)
    (arguments
     (list
      #:install-source? #f
      #:phases
      #~(modify-phases %standard-phases
          (add-before 'configure 'check-lockfile
            (lambda _
              (unless (file-exists? "Cargo.lock")
                (error "missing Cargo.lock")))))))
    (native-inputs (list clang pkg-config rust))
    (inputs (append (cargo-inputs-from-lockfile %spacesniffer1000-lockfile)
                    (list libxkbcommon wayland wayland-protocols vulkan-loader)))
    (synopsis "Native graphical disk space visualizer")
    (description
     "SpaceSniffer1000 is an egui desktop application for exploring filesystem usage with a clickable treemap.")
    (home-page "https://github.com/trevarj/spacesniffer1000")
    (license license:expat)))

spacesniffer1000
