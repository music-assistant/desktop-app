use sendspin::protocol::client_builder::ProtocolClientBuilder;
use sendspin::protocol::messages::{
    ArtworkChannel, ArtworkSource, ArtworkV1Support, AudioFormatSpec, ImageFormat, PlayerV1Support,
    VisualizerDataType, VisualizerV1Support,
};

#[test]
fn test_supported_roles_with_player_v1_support() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .build();

    assert_eq!(builder.supported_roles(), &["player@v1"]);
}

#[test]
fn test_supported_roles_with_artwork_v1_support() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .artwork_v1_support(ArtworkV1Support {
            channels: vec![ArtworkChannel {
                source: ArtworkSource::Album,
                format: ImageFormat::Jpeg,
                media_width: 300,
                media_height: 300,
            }],
        })
        .build();

    assert_eq!(builder.supported_roles(), &["artwork@v1"]);
}

#[test]
fn test_supported_roles_with_metadata_role() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .metadata()
        .build();

    assert_eq!(builder.supported_roles(), &["metadata@v1"]);
    assert!(builder.player_v1_support().is_none());
}

#[test]
fn test_supported_roles_with_metadata_and_artwork() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .metadata()
        .artwork_v1_support(ArtworkV1Support {
            channels: vec![ArtworkChannel {
                source: ArtworkSource::Album,
                format: ImageFormat::Jpeg,
                media_width: 300,
                media_height: 300,
            }],
        })
        .build();

    assert_eq!(builder.supported_roles(), &["artwork@v1", "metadata@v1"]);
    assert!(builder.player_v1_support().is_none());
}

#[test]
fn test_supported_roles_with_visualizer_v1_support() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .visualizer_v1_support(VisualizerV1Support {
            types: vec![VisualizerDataType::Spectrum],
            buffer_capacity: 1024,
            rate_max: 60,
            spectrum: None,
        })
        .build();

    assert_eq!(builder.supported_roles(), &["visualizer@v1"]);
}

#[test]
fn test_supported_roles_with_multiple_supports() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .artwork_v1_support(ArtworkV1Support {
            channels: vec![ArtworkChannel {
                source: ArtworkSource::Album,
                format: ImageFormat::Jpeg,
                media_width: 300,
                media_height: 300,
            }],
        })
        .visualizer_v1_support(VisualizerV1Support {
            types: vec![VisualizerDataType::Loudness],
            buffer_capacity: 1024,
            rate_max: 60,
            spectrum: None,
        })
        .controller()
        .build();

    assert_eq!(
        builder.supported_roles(),
        &["player@v1", "artwork@v1", "visualizer@v1", "controller@v1"]
    );
}

#[test]
fn test_default_player_v1_support_applied_at_build_time() {
    // When no player support is explicitly set, the builder has default player@v1 support
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .build();

    // Default player@v1 role is present
    assert_eq!(builder.supported_roles(), &["player@v1"]);

    // Default player support is applied
    let support = builder
        .player_v1_support()
        .expect("should have default player support");
    assert_eq!(support.supported_formats.len(), 3);
    assert_eq!(support.supported_formats[0].codec, "opus");
    assert_eq!(support.supported_formats[0].channels, 2);
    assert_eq!(support.supported_formats[0].sample_rate, 48000);
    assert_eq!(support.supported_formats[0].bit_depth, 16);
    assert_eq!(support.supported_formats[1].codec, "pcm");
    assert_eq!(support.supported_formats[1].channels, 2);
    assert_eq!(support.supported_formats[1].sample_rate, 48000);
    assert_eq!(support.supported_formats[1].bit_depth, 24);
    assert_eq!(support.supported_formats[2].codec, "pcm");
    assert_eq!(support.supported_formats[2].channels, 2);
    assert_eq!(support.supported_formats[2].sample_rate, 48000);
    assert_eq!(support.supported_formats[2].bit_depth, 16);
    assert_eq!(support.buffer_capacity, 50 * 1024 * 1024); // 50 MB
    assert_eq!(
        support.supported_commands,
        vec!["volume".to_string(), "mute".to_string()]
    );
}

#[test]
fn test_explicit_player_support_is_preserved() {
    let custom_support = PlayerV1Support {
        supported_formats: vec![AudioFormatSpec {
            codec: "opus".to_string(),
            channels: 1,
            sample_rate: 44100,
            bit_depth: 16,
        }],
        buffer_capacity: 1024,
        supported_commands: vec!["pause".to_string()],
    };

    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .player_v1_support(custom_support.clone())
        .build();

    let support = builder
        .player_v1_support()
        .expect("should have player support");
    assert_eq!(support.supported_formats[0].codec, "opus");
    assert_eq!(support.buffer_capacity, 1024);
}

#[test]
fn test_controller_role_added_to_supported_roles() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .controller()
        .build();

    assert_eq!(builder.supported_roles(), &["controller@v1"]);
}

#[test]
fn test_controller_role_not_present_by_default() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .build();

    assert!(!builder
        .supported_roles()
        .contains(&"controller@v1".to_string()));
}

#[test]
fn test_color_role_added_to_supported_roles() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .color()
        .build();

    assert_eq!(builder.supported_roles(), &["color@v1"]);
    assert!(builder.player_v1_support().is_none());
}

#[test]
fn test_color_role_not_present_by_default() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .build();

    assert!(!builder.supported_roles().contains(&"color@v1".to_string()));
}

#[test]
fn test_color_role_combines_with_other_roles() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test".to_string())
        .name("Test Client".to_string())
        .metadata()
        .color()
        .build();

    assert_eq!(builder.supported_roles(), &["metadata@v1", "color@v1"]);
    assert!(builder.player_v1_support().is_none());
}
