mod fixtures;
mod utils;

use std::{collections::HashSet, time::Duration};

use anyhow::{ensure, Result};
use assert_fs::TempDir;
use configparser::ini::Ini;
use fixtures::{consul, tmpdir, ConsulContainer};
use pretty_assertions::assert_eq;
use rstest::rstest;
use tokio::{process::Command, time::sleep};
use wireguard_keys::Privkey;
use wiresmith::{networkd::NetworkdConfiguration, wireguard::WgPeer};

use crate::{utils::wait_for_files, utils::WiresmithContainer};

/// If the address is provided explicitly, it needs to be contained within network.
#[rstest]
#[case("10.0.0.0/24", "10.0.0.1", true)]
#[case("10.0.0.0/24", "10.0.1.1", false)]
#[tokio::test]
async fn address_contained_within_network(
    #[case] network: &str,
    #[case] address: &str,
    #[case] success: bool,
) -> Result<()> {
    let output = assert_cmd::Command::cargo_bin("wiresmith")?
        .arg("--network")
        .arg(network)
        .arg("--address")
        .arg(address)
        .arg("--endpoint-address")
        .arg("dontcare")
        .output()?;
    if !success {
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("Error: Address {address} is not part of network {network}\n")
        );
    }

    Ok(())
}

/// An initial configuration with a single peer is created in case no existing peers are found.
/// The address of the peer is not explicitly provided. Instead, the first free address inside the
/// network is used.
#[rstest]
#[tokio::test]
async fn initial_configuration(#[future] consul: ConsulContainer, tmpdir: TempDir) -> Result<()> {
    let consul = consul.await;

    let wiresmith = WiresmithContainer::new(
        "initial",
        "10.0.0.0/24",
        "192.168.0.1",
        consul.http_port,
        &[],
        &tmpdir,
    )
    .await;

    let network_file = tmpdir.join("wg0.network");
    let netdev_file = tmpdir.join("wg0.netdev");

    wait_for_files(vec![network_file.as_path(), netdev_file.as_path()]).await;

    // Check the networkd files.
    let network_ini = ini::Ini::load_from_file(network_file)?;
    assert_eq!(
        network_ini
            .section(Some("Match"))
            .unwrap()
            .get("Name")
            .unwrap(),
        "wg0"
    );
    assert_eq!(
        network_ini
            .section(Some("Network"))
            .unwrap()
            .get("Address")
            .unwrap(),
        "10.0.0.1/24"
    );

    let netdev_ini = ini::Ini::load_from_file(netdev_file)?;
    assert_eq!(
        netdev_ini
            .section(Some("NetDev"))
            .unwrap()
            .get("Name")
            .unwrap(),
        "wg0"
    );
    assert_eq!(
        netdev_ini
            .section(Some("NetDev"))
            .unwrap()
            .get("Kind")
            .unwrap(),
        "wireguard"
    );
    assert_eq!(
        netdev_ini
            .section(Some("NetDev"))
            .unwrap()
            .get("Description")
            .unwrap(),
        "WireGuard client"
    );
    assert_eq!(
        netdev_ini
            .section(Some("NetDev"))
            .unwrap()
            .get("MTUBytes")
            .unwrap(),
        "1280"
    );

    // The private key is generated automatically but we should verify it's valid.
    let private_key = Privkey::from_base64(
        netdev_ini
            .section(Some("WireGuard"))
            .unwrap()
            .get("PrivateKey")
            .unwrap(),
    )?;

    // Check whether the interface was configured correctly.
    let networkctl_output = Command::new("podman")
        .arg("exec")
        .arg(&wiresmith.container_name)
        .arg("networkctl")
        .arg("status")
        .arg("wg0")
        .output()
        .await?;
    ensure!(
        networkctl_output.stderr.is_empty(),
        "Error running networkctl: {}",
        String::from_utf8_lossy(&networkctl_output.stderr)
    );
    let wg_showconf_output = Command::new("podman")
        .arg("exec")
        .arg(&wiresmith.container_name)
        .arg("wg")
        .arg("showconf")
        .arg("wg0")
        .output()
        .await?;
    ensure!(
        wg_showconf_output.stderr.is_empty(),
        "Error running wg showconf: {}",
        String::from_utf8_lossy(&wg_showconf_output.stderr)
    );
    dbg!(String::from_utf8_lossy(&wg_showconf_output.stdout));
    let mut wg_config = Ini::new();
    wg_config
        .read(String::from_utf8_lossy(&wg_showconf_output.stdout).to_string())
        .expect("Couldn't parse WireGuard config");
    assert_eq!(wg_config.get("Interface", "ListenPort").unwrap(), "51820");
    assert_eq!(
        wg_config.get("Interface", "PrivateKey").unwrap(),
        private_key.to_base64()
    );

    // There should be no peers here yet.
    assert!(!wg_config.sections().contains(&"Peer".to_string()));

    // Check the config put into Consul.
    let peers = consul.client.get_peers().await?;
    let mut expected_peers = HashSet::new();
    expected_peers.insert(WgPeer {
        public_key: private_key.pubkey(),
        endpoint: "192.168.0.1:51820".parse().unwrap(),
        address: "10.0.0.1/32".parse().unwrap(),
    });

    // Now there should be peers.
    assert_eq!(peers, expected_peers);

    Ok(())
}

/// A second peer is joining the network after the first one has created the initial configuration.
/// This should cause the first peer to generate a new network config with the new peer. The second
/// peer should generate a network config containing the first peer.
/// Afterwards, a third peers joins and the previous two nodes should be notified of that.
#[rstest]
#[tokio::test]
async fn join_network(
    #[future] consul: ConsulContainer,
    #[from(tmpdir)] tmpdir_a: TempDir,
    #[from(tmpdir)] tmpdir_b: TempDir,
    #[from(tmpdir)] tmpdir_c: TempDir,
) -> Result<()> {
    let consul = consul.await;

    let _wiresmith_a = WiresmithContainer::new(
        "a",
        "10.0.0.0/24",
        "192.168.0.1",
        consul.http_port,
        &[],
        &tmpdir_a,
    )
    .await;

    // let network_file_a = tmpdir_a.join("wg0.network");
    // let netdev_file_a = tmpdir_a.join("wg0.netdev");

    // wait_for_files(vec![network_file_a.as_path(), netdev_file_a.as_path()]).await;

    // // We should now have some initial configuration with an empty list of peers.
    // let networkd_config_a = NetworkdConfiguration::from_config(&tmpdir_a, "wg0").await?;
    // assert_eq!(networkd_config_a.wg_address, "10.0.0.1/24".parse()?);
    // assert!(networkd_config_a.peers.is_empty());

    // Start the second peer after the first one has generated its files so we don't run into race
    // conditions with address allocation.
    let _wiresmith_b = WiresmithContainer::new(
        "b",
        "10.0.0.0/24",
        "192.168.0.2",
        consul.http_port,
        &[],
        &tmpdir_b,
    )
    .await;

    // let network_file_b = tmpdir_b.join("wg0.network");
    // let netdev_file_b = tmpdir_b.join("wg0.netdev");

    // wait_for_files(vec![network_file_b.as_path(), netdev_file_b.as_path()]).await;

    // // Wait until the first client has had a chance to pick up the changes and generate a new
    // // config. If this is flaky, increase this number slightly.
    // sleep(Duration::from_secs(2)).await;

    // let networkd_config_a = NetworkdConfiguration::from_config(&tmpdir_a, "wg0").await?;
    // let networkd_config_b = NetworkdConfiguration::from_config(&tmpdir_b, "wg0").await?;

    // assert_eq!(networkd_config_a.wg_address, "10.0.0.1/24".parse()?);
    // assert_eq!(networkd_config_b.wg_address, "10.0.0.2/24".parse()?);

    // // We don't expect to see ourselves in the list of peers as we don't want to peer with
    // // ourselves.
    // let mut expected_peers_a = HashSet::new();
    // expected_peers_a.insert(WgPeer {
    //     public_key: networkd_config_b.public_key,
    //     endpoint: "192.168.0.2:51820".parse().unwrap(),
    //     address: "10.0.0.2/32".parse().unwrap(),
    // });

    // let mut expected_peers_b = HashSet::new();
    // expected_peers_b.insert(WgPeer {
    //     public_key: networkd_config_a.public_key,
    //     endpoint: "192.168.0.1:51820".parse().unwrap(),
    //     address: "10.0.0.1/32".parse().unwrap(),
    // });
    // assert_eq!(networkd_config_a.peers, expected_peers_a);
    // assert_eq!(networkd_config_b.peers, expected_peers_b);

    // // Peers in Consul should be union the other peer lists.
    // let consul_peers = consul.client.get_peers().await?;
    // let expected_peers = networkd_config_a
    //     .peers
    //     .union(&networkd_config_b.peers)
    //     .cloned()
    //     .collect::<HashSet<_>>();

    // assert_eq!(consul_peers, expected_peers);

    // The third peer now joins.
    let _wiresmith_c = WiresmithContainer::new(
        "c",
        "10.0.0.0/24",
        "192.168.0.3",
        consul.http_port,
        &[],
        &tmpdir_c,
    )
    .await;

    // let network_file_c = tmpdir_c.join("wg0.network");
    // let netdev_file_c = tmpdir_c.join("wg0.netdev");

    // wait_for_files(vec![network_file_c.as_path(), netdev_file_c.as_path()]).await;

    // Wait again for clients to pick up changes.
    sleep(Duration::from_secs(2000)).await;

    let networkd_config_a = NetworkdConfiguration::from_config(&tmpdir_a, "wg0").await?;
    let networkd_config_b = NetworkdConfiguration::from_config(&tmpdir_b, "wg0").await?;
    let networkd_config_c = NetworkdConfiguration::from_config(&tmpdir_c, "wg0").await?;

    assert_eq!(networkd_config_a.wg_address, "10.0.0.1/24".parse()?);
    assert_eq!(networkd_config_b.wg_address, "10.0.0.2/24".parse()?);
    assert_eq!(networkd_config_c.wg_address, "10.0.0.3/24".parse()?);

    // We recheck that now everyone has everyone else but not themselves.
    let mut expected_peers_a = HashSet::new();
    expected_peers_a.insert(WgPeer {
        public_key: networkd_config_b.public_key,
        endpoint: "192.168.0.2:51820".parse().unwrap(),
        address: "10.0.0.2/32".parse().unwrap(),
    });
    expected_peers_a.insert(WgPeer {
        public_key: networkd_config_c.public_key,
        endpoint: "192.168.0.3:51820".parse().unwrap(),
        address: "10.0.0.3/32".parse().unwrap(),
    });

    let mut expected_peers_b = HashSet::new();
    expected_peers_b.insert(WgPeer {
        public_key: networkd_config_a.public_key,
        endpoint: "192.168.0.1:51820".parse().unwrap(),
        address: "10.0.0.1/32".parse().unwrap(),
    });
    expected_peers_b.insert(WgPeer {
        public_key: networkd_config_c.public_key,
        endpoint: "192.168.0.3:51820".parse().unwrap(),
        address: "10.0.0.3/32".parse().unwrap(),
    });

    let mut expected_peers_c = HashSet::new();
    expected_peers_c.insert(WgPeer {
        public_key: networkd_config_a.public_key,
        endpoint: "192.168.0.1:51820".parse().unwrap(),
        address: "10.0.0.1/32".parse().unwrap(),
    });
    expected_peers_c.insert(WgPeer {
        public_key: networkd_config_b.public_key,
        endpoint: "192.168.0.2:51820".parse().unwrap(),
        address: "10.0.0.2/32".parse().unwrap(),
    });
    assert_eq!(networkd_config_a.peers, expected_peers_a);
    assert_eq!(networkd_config_b.peers, expected_peers_b);
    assert_eq!(networkd_config_c.peers, expected_peers_c);

    // Peers in Consul should be union the other peer lists.
    let consul_peers = consul.client.get_peers().await?;
    let expected_peers = networkd_config_a
        .peers
        .union(&networkd_config_b.peers)
        .cloned()
        .collect::<HashSet<_>>()
        .union(&networkd_config_c.peers)
        .cloned()
        .collect::<HashSet<_>>();

    assert_eq!(consul_peers, expected_peers);

    Ok(())
}
