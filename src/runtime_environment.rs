use std::io;
use std::path::{Path, PathBuf};

const APPLICATION_TEMP_DIRECTORY_NAME: &str = "RemoteControlMCP";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeInfo {
    pub description: String,
    pub is_elevated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEnvironment {
    pub user: String,
    pub privilege: PrivilegeInfo,
    pub privilege_detection_warning: Option<String>,
    pub working_directory: PathBuf,
}

pub fn application_temp_directory() -> PathBuf {
    application_temp_directory_from(&std::env::temp_dir())
}

fn application_temp_directory_from(temp_directory: &Path) -> PathBuf {
    temp_directory.join(APPLICATION_TEMP_DIRECTORY_NAME)
}

pub fn initialize() -> io::Result<RuntimeEnvironment> {
    let requested_working_directory = application_temp_directory();
    std::fs::create_dir_all(&requested_working_directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not create the application temporary directory {}: {error}",
                requested_working_directory.display()
            ),
        )
    })?;
    std::env::set_current_dir(&requested_working_directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Could not change the application working directory to {}: {error}",
                requested_working_directory.display()
            ),
        )
    })?;
    let working_directory = std::env::current_dir().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("Could not read the application working directory: {error}"),
        )
    })?;

    let (user, privilege, privilege_detection_warning) = detect_runtime_identity();
    Ok(RuntimeEnvironment {
        user,
        privilege,
        privilege_detection_warning,
        working_directory,
    })
}

#[cfg(any(unix, test))]
fn unix_privilege_info(effective_uid: u32) -> PrivilegeInfo {
    if effective_uid == 0 {
        PrivilegeInfo {
            description: "Root (effective UID 0)".to_string(),
            is_elevated: true,
        }
    } else {
        PrivilegeInfo {
            description: format!("Standard user (effective UID {effective_uid})"),
            is_elevated: false,
        }
    }
}

#[cfg(any(windows, test))]
fn windows_privilege_info(is_elevated: bool) -> PrivilegeInfo {
    if is_elevated {
        PrivilegeInfo {
            description: "Elevated token".to_string(),
            is_elevated: true,
        }
    } else {
        PrivilegeInfo {
            description: "Standard token".to_string(),
            is_elevated: false,
        }
    }
}

#[cfg(unix)]
fn detect_runtime_identity() -> (String, PrivilegeInfo, Option<String>) {
    let effective_uid = unsafe { libc::geteuid() };
    let user = unix_user_name(effective_uid).unwrap_or_else(|| format!("UID {effective_uid}"));
    (user, unix_privilege_info(effective_uid), None)
}

#[cfg(unix)]
fn unix_user_name(user_id: libc::uid_t) -> Option<String> {
    const MAXIMUM_BUFFER_SIZE: usize = 1024 * 1024;

    let recommended_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if recommended_size > 0 {
        recommended_size as usize
    } else {
        1024
    };

    loop {
        let mut password_entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getpwuid_r(
                user_id,
                password_entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };

        if status == 0 {
            if result.is_null() {
                return None;
            }
            let password_entry = unsafe { password_entry.assume_init() };
            if password_entry.pw_name.is_null() {
                return None;
            }
            return Some(
                unsafe { std::ffi::CStr::from_ptr(password_entry.pw_name) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if status != libc::ERANGE || buffer_size >= MAXIMUM_BUFFER_SIZE {
            return None;
        }
        buffer_size = (buffer_size * 2).min(MAXIMUM_BUFFER_SIZE);
    }
}

#[cfg(windows)]
fn detect_runtime_identity() -> (String, PrivilegeInfo, Option<String>) {
    let mut warnings = Vec::new();
    let user = match windows_user_name() {
        Ok(user) => user,
        Err(error) => {
            warnings.push(error.clone());
            format!("Unknown user ({error})")
        }
    };
    let privilege = match windows_token_is_elevated() {
        Ok(is_elevated) => windows_privilege_info(is_elevated),
        Err(error) => {
            warnings.push(error);
            PrivilegeInfo {
                description: "Unknown token permissions".to_string(),
                is_elevated: false,
            }
        }
    };
    let warning = (!warnings.is_empty()).then(|| warnings.join(" "));
    (user, privilege, warning)
}

#[cfg(windows)]
fn windows_user_name() -> Result<String, String> {
    use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;

    const MAXIMUM_USER_NAME_LENGTH_WITH_TERMINATOR: usize = 257;
    let mut buffer = [0_u16; MAXIMUM_USER_NAME_LENGTH_WITH_TERMINATOR];
    let mut length = buffer.len() as u32;
    if unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(format!(
            "Could not determine the Windows user name: {}",
            io::Error::last_os_error()
        ));
    }
    let length_without_terminator = length.saturating_sub(1) as usize;
    Ok(String::from_utf16_lossy(
        &buffer[..length_without_terminator],
    ))
}

#[cfg(windows)]
fn windows_token_is_elevated() -> Result<bool, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!(
            "Could not open the Windows process token: {}",
            io::Error::last_os_error()
        ));
    }

    let result = (|| {
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned_length = 0;
        if unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned_length,
            )
        } == 0
        {
            return Err(format!(
                "Could not inspect the Windows process token: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(elevation.TokenIsElevated != 0)
    })();

    unsafe {
        CloseHandle(token);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_temp_directory_uses_remote_control_mcp_subdirectory() {
        assert_eq!(
            application_temp_directory_from(Path::new("test-temp")),
            Path::new("test-temp").join("RemoteControlMCP")
        );
    }

    #[test]
    fn unix_effective_uid_zero_is_root() {
        assert_eq!(
            unix_privilege_info(0),
            PrivilegeInfo {
                description: "Root (effective UID 0)".to_string(),
                is_elevated: true,
            }
        );
    }

    #[test]
    fn unix_nonzero_effective_uid_is_standard() {
        assert_eq!(
            unix_privilege_info(1000),
            PrivilegeInfo {
                description: "Standard user (effective UID 1000)".to_string(),
                is_elevated: false,
            }
        );
    }

    #[test]
    fn windows_token_elevation_is_reported() {
        assert_eq!(
            windows_privilege_info(true),
            PrivilegeInfo {
                description: "Elevated token".to_string(),
                is_elevated: true,
            }
        );
        assert_eq!(
            windows_privilege_info(false),
            PrivilegeInfo {
                description: "Standard token".to_string(),
                is_elevated: false,
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_runtime_identity_apis_are_available() {
        assert!(!windows_user_name().unwrap().is_empty());
        windows_token_is_elevated().unwrap();
    }
}
