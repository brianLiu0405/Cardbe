import { m } from "#lib/paraglide/messages.js";
import type { EnterpriseErrorCode } from "./client.js";
import type { BoardRole, OrganizationRole } from "./types.js";

export function roleLabel(role: OrganizationRole | BoardRole): string {
  switch (role) {
    case "owner":
      return m.enterprise_role_owner();
    case "admin":
      return m.enterprise_role_admin();
    case "member":
      return m.enterprise_role_member();
    case "manager":
      return m.enterprise_role_manager();
    case "editor":
      return m.enterprise_role_editor();
    case "viewer":
      return m.enterprise_role_viewer();
  }
}

export function errorLabel(code: EnterpriseErrorCode): string {
  switch (code) {
    case "UNAUTHENTICATED":
    case "SESSION_ENDED":
      return m.enterprise_error_unauthenticated();
    case "PERMISSION_DENIED":
      return m.enterprise_error_permission();
    case "NOT_FOUND":
      return m.enterprise_error_not_found();
    case "INVALID_ARGUMENT":
      return m.enterprise_error_invalid();
    case "CONFLICT":
      return m.enterprise_error_conflict();
    case "IDEMPOTENCY_CONFLICT":
      return m.enterprise_error_idempotency();
    case "LAST_MANAGER":
      return m.enterprise_error_last_manager();
    case "NETWORK_ERROR":
      return m.enterprise_error_network();
    default:
      return m.enterprise_error_internal();
  }
}
