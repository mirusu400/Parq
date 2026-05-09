import { invoke } from "@tauri-apps/api/core";
import type { Disk } from "../types";

// 백엔드 commands::read::list_disks 의 클라이언트 래퍼.
// 백엔드는 read-only 로 WMI(MSFT_Disk/Partition/Volume) 를 쿼리한다.
// 에러는 Rust 의 ParqError 를 사람이 읽을 수 있는 한국어 문자열로 변환한 것이다.
export async function listDisks(): Promise<Disk[]> {
  return invoke<Disk[]>("list_disks");
}
