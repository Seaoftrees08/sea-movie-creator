//! `sea-movie-creator` アプリケーションのエントリーポイント。

use sea_movie_creator::APPLICATION_NAME;
use sea_movie_creator::core::frame_rate::calculate_total_frames;

fn main() {
    // Phase 0 の疎通確認用エントリ。本格的な GUI 起動処理は Phase A 以降で実装する。
    let initial_frame_count = calculate_total_frames(1_000, 60).unwrap_or(0);
    println!("{APPLICATION_NAME} initialized (sample frames: {initial_frame_count})");
}
