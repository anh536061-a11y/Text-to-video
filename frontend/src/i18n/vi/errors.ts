import type enErrors from '../en/errors';

export default {
  'unknown_error': 'Đã xảy ra lỗi không xác định',
  'network_error': 'Lỗi mạng, vui lòng kiểm tra kết nối',
  'unauthorized': 'Không được phép, vui lòng đăng nhập lại',
  'forbidden': 'Không có quyền truy cập',
  'not_found': 'Không tìm thấy tài nguyên',
  'server_error': 'Lỗi máy chủ, vui lòng thử lại sau',
  'validation_error': 'Xác thực thất bại',
  'source_unsupported_format': 'Định dạng nguồn không được hỗ trợ: {{ext}}',
  'source_decode_failed': 'Không thể giải mã "{{filename}}" (đã thử: {{tried}})',
  'source_corrupt_file': 'Tệp nguồn "{{filename}}" không thể phân tích: {{reason}}',
  'source_too_large': 'Tệp nguồn "{{filename}}" quá lớn ({{size_mb}} MB > {{limit_mb}} MB)',
  'source_conflict': 'Tệp nguồn "{{existing}}" đã tồn tại',
} satisfies Record<keyof typeof enErrors, string>;
