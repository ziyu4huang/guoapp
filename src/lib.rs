//! guo — 红果短剧工具核心库：App API（X-Gorgon 签名）、网页回退、备用中继、
//! CENC 解密、HLS 捆绑下载、状态持久化，以及 egui 原生 GUI。

pub mod backup;
pub mod browser_cookies;
pub mod cenc;
pub mod gui;
pub mod hgapp;
pub mod hgweb;
pub mod jsonx;
pub mod media;
pub mod model;
pub mod player;
pub mod sign;
pub mod state;
