use sea_movie_creator::APPLICATION_NAME;
use sea_movie_creator::core::frame_rate::calculate_total_frames;

#[test]
fn library_exposes_app_name_and_frame_calculation() {
    assert_eq!(APPLICATION_NAME, "sea-movie-creator");
    assert_eq!(calculate_total_frames(10_000, 30), Ok(300));
}
