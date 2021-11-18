use std::collections::HashMap;

use log::{debug, error, info};
use serde::{
    ser::SerializeSeq, Deserialize, Deserializer, Serialize, Serializer,
};

use crate::{
    ifaces::inter_ifaces_controller::{
        handle_changed_ports, set_ifaces_up_priority,
    },
    ErrorKind, Interface, InterfaceState, InterfaceType, NmstateError,
};

// The max loop count for Interfaces.set_up_priority()
// This allows interface with 4 nested levels in any order.
// To support more nested level, user could place top controller at the
// beginning of desire state
const INTERFACES_SET_PRIORITY_MAX_RETRY: u32 = 4;

#[derive(Clone, Debug, Default)]
pub struct Interfaces {
    pub(crate) kernel_ifaces: HashMap<String, Interface>,
    pub(crate) user_ifaces: HashMap<(String, InterfaceType), Interface>,
    // The insert_order is allowing user to provided ordered interface
    // to support 5+ nested dependency.
    pub(crate) insert_order: Vec<(String, InterfaceType)>,
}

impl<'de> Deserialize<'de> for Interfaces {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let mut ret = Self::new();
        let ifaces =
            <Vec<Interface> as Deserialize>::deserialize(deserializer)?;
        for iface in ifaces {
            ret.push(iface)
        }
        Ok(ret)
    }
}

impl Serialize for Interfaces {
    // Serialize is also used for verification.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ifaces = self.to_vec();
        let mut seq = serializer.serialize_seq(Some(ifaces.len()))?;
        for iface in ifaces {
            seq.serialize_element(iface)?;
        }
        seq.end()
    }
}

impl Interfaces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn to_vec(&self) -> Vec<&Interface> {
        let mut ifaces = Vec::new();
        for iface in self.kernel_ifaces.values() {
            ifaces.push(iface);
        }
        for iface in self.user_ifaces.values() {
            ifaces.push(iface);
        }
        ifaces.sort_unstable_by_key(|iface| iface.name());
        // Use sort_by_key() instead of unstable one, do we can alphabet
        // activation order which is required to simulate the OS boot-up.
        ifaces.sort_by_key(|iface| iface.base_iface().up_priority);

        ifaces
    }

    pub(crate) fn get_iface<'a, 'b>(
        &'a self,
        iface_name: &'b str,
        iface_type: InterfaceType,
    ) -> Option<&'a Interface> {
        if iface_type == InterfaceType::Unknown {
            self.kernel_ifaces.get(&iface_name.to_string()).or_else(|| {
                for iface in self.user_ifaces.values() {
                    if iface.name() == iface_name {
                        return Some(iface);
                    }
                }
                None
            })
        } else if iface_type.is_userspace() {
            self.user_ifaces.get(&(iface_name.to_string(), iface_type))
        } else {
            self.kernel_ifaces.get(&iface_name.to_string())
        }
    }

    fn get_iface_mut<'a, 'b>(
        &'a mut self,
        iface_name: &'b str,
        iface_type: InterfaceType,
    ) -> Option<&'a mut Interface> {
        if iface_type.is_userspace() {
            self.user_ifaces
                .get_mut(&(iface_name.to_string(), iface_type))
        } else {
            self.kernel_ifaces.get_mut(&iface_name.to_string())
        }
    }

    pub fn push(&mut self, iface: Interface) {
        self.insert_order
            .push((iface.name().to_string(), iface.iface_type()));
        if iface.is_userspace() {
            self.user_ifaces
                .insert((iface.name().to_string(), iface.iface_type()), iface);
        } else {
            self.kernel_ifaces.insert(iface.name().to_string(), iface);
        }
    }

    pub fn update(&mut self, other: &Self) {
        let mut new_ifaces: Vec<&Interface> = Vec::new();
        let other_ifaces = other.to_vec();
        for other_iface in &other_ifaces {
            match self
                .get_iface_mut(other_iface.name(), other_iface.iface_type())
            {
                Some(self_iface) => {
                    debug!(
                        "Merging interface {:?} into {:?}",
                        other_iface, self_iface
                    );
                    self_iface.update(other_iface);
                }
                None => {
                    debug!("Appending new interface {:?}", other_iface);
                    new_ifaces.push(other_iface);
                }
            }
        }
        for new_iface in new_ifaces {
            self.push(new_iface.clone());
        }
    }

    pub(crate) fn verify(&self, cur_ifaces: &Self) -> Result<(), NmstateError> {
        for iface in self.to_vec() {
            if iface.is_absent() {
                if let Some(cur_iface) =
                    cur_ifaces.get_iface(iface.name(), iface.iface_type())
                {
                    verify_desire_absent_but_found_in_current(
                        iface, cur_iface,
                    )?;
                }
            } else if let Some(cur_iface) =
                cur_ifaces.get_iface(iface.name(), iface.iface_type())
            {
                iface.verify(cur_iface)?;
            } else {
                return Err(NmstateError::new(
                    ErrorKind::VerificationError,
                    format!(
                        "Failed to find desired interface {} {:?}",
                        iface.name(),
                        iface.iface_type()
                    ),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn gen_state_for_apply(
        &mut self,
        current: &Self,
    ) -> Result<(Self, Self, Self), NmstateError> {
        let mut add_ifaces = Self::new();
        let mut chg_ifaces = Self::new();
        let mut del_ifaces = Self::new();

        resolve_unknown_ifaces(self, current)?;
        handle_changed_ports(self, current)?;
        self.set_up_priority()?;

        for iface in self.to_vec() {
            if iface.is_absent() {
                for del_iface in gen_ifaces_to_del(iface, current) {
                    del_ifaces.push(del_iface);
                }
            } else {
                match current.kernel_ifaces.get(iface.name()) {
                    Some(cur_iface) => {
                        let mut chg_iface = iface.clone();
                        chg_iface.set_iface_type(cur_iface.iface_type());
                        chg_iface.pre_edit_cleanup()?;
                        info!(
                            "Changing interface {} with type {}",
                            chg_iface.name(),
                            chg_iface.iface_type()
                        );
                        chg_ifaces.push(chg_iface);
                    }
                    None => {
                        let mut new_iface = iface.clone();
                        new_iface.pre_edit_cleanup()?;
                        info!(
                            "Adding interface {} with type {}",
                            new_iface.name(),
                            new_iface.iface_type()
                        );
                        add_ifaces.push(new_iface);
                    }
                }
            }
        }

        Ok((add_ifaces, chg_ifaces, del_ifaces))
    }

    pub fn set_up_priority(&mut self) -> Result<(), NmstateError> {
        for _ in 0..INTERFACES_SET_PRIORITY_MAX_RETRY {
            if set_ifaces_up_priority(self) {
                return Ok(());
            }
        }
        error!(
            "Failed to set up priority: please order the interfaces in desire \
            state to place controller before its ports"
        );
        Err(NmstateError::new(
            ErrorKind::InvalidArgument,
            "Failed to set up priority: nmstate only support nested interface \
            up to 4 levels. To support more nest level, \
            please order the interfaces in desire \
            state to place controller before its ports"
                .to_string(),
        ))
    }
}

fn verify_desire_absent_but_found_in_current(
    des_iface: &Interface,
    cur_iface: &Interface,
) -> Result<(), NmstateError> {
    if cur_iface.is_virtual() {
        // Virtual interface should be deleted by absent action
        let e = NmstateError::new(
            ErrorKind::VerificationError,
            format!(
                "Absent interface {}/{} still found as {:?}",
                des_iface.name(),
                des_iface.iface_type(),
                cur_iface
            ),
        );
        error!("{}", e);
        Err(e)
    } else if cur_iface.is_up() {
        // Real hardware should be marked as down by absent action
        let e = NmstateError::new(
            ErrorKind::VerificationError,
            format!(
                "Absent interface {}/{} still found as \
                state up: {:?}",
                des_iface.name(),
                des_iface.iface_type(),
                cur_iface
            ),
        );
        error!("{}", e);
        Err(e)
    } else {
        Ok(())
    }
}

fn gen_ifaces_to_del(
    del_iface: &Interface,
    cur_ifaces: &Interfaces,
) -> Vec<Interface> {
    let mut del_ifaces = Vec::new();
    let cur_ifaces = cur_ifaces.to_vec();
    for cur_iface in cur_ifaces {
        if cur_iface.name() == del_iface.name()
            && (del_iface.iface_type() == InterfaceType::Unknown
                || del_iface.iface_type() == cur_iface.iface_type())
        {
            let mut tmp_iface = del_iface.clone();
            tmp_iface.base_iface_mut().iface_type = cur_iface.iface_type();
            info!(
                "Deleting interface {}/{}",
                tmp_iface.name(),
                tmp_iface.iface_type()
            );
            del_ifaces.push(tmp_iface);
        }
    }
    del_ifaces
}

fn resolve_unknown_ifaces(
    ifaces: &mut Interfaces,
    cur_ifaces: &Interfaces,
) -> Result<(), NmstateError> {
    let mut resolved_ifaces: Vec<Interface> = Vec::new();
    for ((iface_name, iface_type), iface) in ifaces.user_ifaces.iter() {
        if iface_type != &InterfaceType::Unknown {
            continue;
        }

        if iface.is_absent() {
            for cur_iface in cur_ifaces.to_vec() {
                if cur_iface.name() == iface_name {
                    let mut new_iface = cur_iface.clone();
                    new_iface.base_iface_mut().state = InterfaceState::Absent;
                    resolved_ifaces.push(new_iface);
                }
            }
        } else {
            let mut found_iface = Vec::new();
            for cur_iface in cur_ifaces.to_vec() {
                if cur_iface.name() == iface_name {
                    let mut new_iface = iface.clone();
                    new_iface.base_iface_mut().iface_type =
                        cur_iface.iface_type().clone();
                    found_iface.push(new_iface);
                }
            }
            match found_iface.len() {
                0 => {
                    let e = NmstateError::new(
                        ErrorKind::InvalidArgument,
                        format!(
                            "Failed to find unknown type interface {} \
                            in current state",
                            iface_name
                        ),
                    );
                    error!("{}", e);
                    return Err(e);
                }
                1 => {
                    let new_iface = Interface::deserialize(
                        serde_json::to_value(&found_iface[0])?,
                    )?;

                    resolved_ifaces.push(new_iface);
                }
                _ => {
                    let e = NmstateError::new(
                        ErrorKind::InvalidArgument,
                        format!(
                            "Found 2+ interface matching desired unknown \
                            type interface {}: {:?}",
                            iface_name, &found_iface
                        ),
                    );
                    error!("{}", e);
                    return Err(e);
                }
            }
        }
    }

    for new_iface in resolved_ifaces {
        ifaces
            .user_ifaces
            .remove(&(new_iface.name().to_string(), InterfaceType::Unknown));
        ifaces.push(new_iface);
    }
    Ok(())
}