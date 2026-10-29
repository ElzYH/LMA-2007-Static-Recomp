//! Guest functions the runtime may replace natively (`runtime::hooks`, #6). Shared with the
//! recompiler: calls to these must always go through the block table (where the runtime leaves
//! them out), never a direct same-part call that would bypass the hook.

/// D3D runtime entry points: DrawVertices, DrawIndexedVertices, DrawVerticesUP, Resolve, Swap;
/// then the club badge setter (`runtime::hooks::BADGE_SET`, mod badges), the club bank
/// balance adder (`runtime::hooks::ADD_FUNDS`, mod `income_scale`), the player valuation and the
/// stored value's setter (`runtime::hooks::VALUATION`, `VALUE_SET`, mod `values_as_stored`) and the
/// weekly wage (`runtime::hooks::WAGE`, mod wages.csv) and the bank statement line
/// (`runtime::hooks::STATEMENT`, mod `income_scale`), and the contract length setter
/// (`runtime::hooks::CONTRACT_SET`, mod wages.csv), and the club funds (`runtime::hooks::FUNDS`, mod funds.csv), and the D3D fence-wait watchdog
/// (`runtime::hooks::FENCE_WATCHDOG`, LMA_FENCE_NAP=1, #312), and the sponsorship deal amount getters and
/// setters (`runtime::hooks::SPONSOR_*`, mod `sponsor_unit`), and a club's wage budget setter
/// (`runtime::hooks::WAGE_BUDGET_SET`, mod wage_budgets.csv), and the club badge getter
/// (`runtime::hooks::BADGE_GET`, mod badges beyond entry 511), and the player's club lookup
/// (`runtime::hooks::PLAYER_CLUB`, mod squad_overflow.csv, #538), and the wage budget's getter
/// (`runtime::hooks::WAGE_BUDGET_GET`, mod wage_budgets.csv: the floor from a career's first day),
/// and a player's overall and role (`runtime::hooks::PLAYER_OVERALL`, `PLAYER_ROLE`,
/// LMA_NATIVE_OVERALL=1, #39), and a league's European places (`runtime::hooks::EUROPE_PLACES`,
/// mod `europe_places`, #607), and the AI transfer search's player filter
/// (`runtime::hooks::SEARCH_FILTER`, #39), the match's slot lookup and builder (mod `bench`,
/// `runtime::hooks::SLOT_LOOKUP`, `MATCH_BUILD`, #530), and the wage rounding and the AI's wage
/// offer (mod `ai_wage_offers`, `runtime::hooks::WAGE_ROUND`, `AI_OFFER`), and the tactics record's
/// per-formation position offsets and the pitch diagram's slot placement (mod formations.csv,
/// `runtime::hooks::FORMATION_OFFSET_*`, `FORMATION_DIAGRAM_SLOT`, #749), the wage from a player's
/// rating (mod `rating_wage_scale`, `runtime::hooks::RATING_WAGE`, #704), and the team screen's
/// setup and its row lookups (mod `bench`, `runtime::hooks::TEAM_SCREEN_SETUP`,
/// `TEAM_ROW_PLAYER`, `TEAM_ROW_STATUS`, #530), and the European Cup's draw, its group stage's
/// end and the cup screens' standings reader (mod `league_phase`, `runtime::hooks::EUROPEAN_CUP_DRAW`,
/// `GROUP_END`, `STANDINGS_READ`, #703).
pub const ADDRESSES: &[u32] = &[0x8233_98A0, 0x8233_9CA8, 0x8233_93D0, 0x8234_02F8, 0x8233_6958, 0x820E_39D8, 0x820E_4018, 0x8212_08E0, 0x8211_DCB0, 0x8212_3DC8, 0x8210_4CD8, 0x8211_C658, 0x820E_AA10, 0x8233_9080, 0x8210_75A0, 0x8210_7608, 0x8210_71C8, 0x8210_7200, 0x8210_7DB8, 0x8210_7E60, 0x820E_6BE0, 0x820E_3970, 0x8211_8FC0, 0x820E_6B80, 0x8211_F4C8, 0x8211_BE88, 0x8213_8E68, 0x8217_94C8, 0x8225_EDF0, 0x8225_BF10, 0x8211_8E88, 0x8218_7738, 0x8225_EDC0, 0x8210_F330, 0x8210_F3F8, 0x8210_F4A8, 0x8210_F570, 0x8219_1B40, 0x8212_0D38, 0x8218_F068, 0x8218_F750, 0x8219_0BC8, 0x8213_94A8, 0x8213_A2D0, 0x820D_F370];
