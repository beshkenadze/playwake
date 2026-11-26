import ProjectDescription

let project = Project(
    name: "PlaywakeIOS",
    packages: [
        // Same libwebrtc build the XcodeGen project used.
        .package(
            url: "https://github.com/stasel/WebRTC.git",
            from: "158.0.0"
        )
    ],
    settings: .settings(
        base: [
            "SWIFT_VERSION": "5.0",
            "SWIFT_STRICT_CONCURRENCY": "minimal",
            "ENABLE_USER_SCRIPT_SANDBOXING": "NO",
        ]
    ),
    targets: [
        .target(
            name: "PlaywakeIOS",
            destinations: [.iPhone, .iPad, .macCatalyst],
            product: .app,
            bundleId: "io.github.beshkenadze.playwake.ios",
            deploymentTargets: .iOS("17.0"),
            infoPlist: .file(path: "PlaywakeIOS/Info.plist"),
            sources: [
                .glob(
                    "PlaywakeIOS/**",
                    excluding: [
                        "PlaywakeIOS/Info.plist",
                        "PlaywakeIOS/Media/README.md",
                    ]
                )
            ],
            scripts: [
                .pre(
                    script: "\"$SRCROOT/scripts/build-rust-core.sh\"",
                    name: "Build Rust core",
                    basedOnDependencyAnalysis: false
                )
            ],
            dependencies: [
                .package(product: "WebRTC")
            ],
            settings: .settings(
                base: [
                    "PRODUCT_NAME": "Playwake",
                    "CODE_SIGN_STYLE": "Automatic",
                    "DEVELOPMENT_TEAM": "Q8H6GWJ658",
                    "CODE_SIGN_IDENTITY": "Apple Development",
                    // Tuist defaults Mac Catalyst to ad-hoc ("-").
                    "CODE_SIGN_IDENTITY[sdk=macosx*]": "Apple Development",
                    "TARGETED_DEVICE_FAMILY": "1,2",
                    "SUPPORTS_MACCATALYST": "YES",
                    "ENABLE_PREVIEWS": "YES",
                    "ASSETCATALOG_COMPILER_APPICON_NAME": "",
                    "SWIFT_OBJC_BRIDGING_HEADER": "PlaywakeIOS/Core/playwake_core.h",
                    "LIBRARY_SEARCH_PATHS": ["$(SRCROOT)/Rust"],
                    "OTHER_LDFLAGS": [
                        "-lplaywake_ffi",
                        "-framework", "Security",
                        "-framework", "CoreFoundation",
                    ],
                ]
            )
        )
    ],
    schemes: [
        .scheme(
            name: "PlaywakeIOS",
            buildAction: .buildAction(targets: ["PlaywakeIOS"]),
            runAction: .runAction(
                configuration: .debug,
                arguments: .arguments(
                    launchArguments: [.launchArgument(name: "-autolab", isEnabled: true)]
                )
            )
        )
    ]
)
