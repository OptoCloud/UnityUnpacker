package main

import (
	"archive/tar"
	"compress/gzip"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
)

type UnityAsset struct {
	AssetPath string
	MetaPath  string
	PathName  string
}

// extractTarGz reads .tar.gz and returns a list of UnityAssets (pathname, asset path, meta path).
func extractTarGz(srcFile, tempDir string) ([]UnityAsset, error) {
	fmt.Printf("[INFO] Extracting archive: %s\n", srcFile)

	file, err := os.Open(srcFile)
	if err != nil {
		return nil, fmt.Errorf("failed to open file: %w", err)
	}
	defer file.Close()

	gzipReader, err := gzip.NewReader(file)
	if err != nil {
		return nil, fmt.Errorf("failed to create gzip reader: %w", err)
	}
	defer gzipReader.Close()

	tarReader := tar.NewReader(gzipReader)

	var current UnityAsset
	var assets []UnityAsset

	for {
		header, err := tarReader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("failed to read tar archive: %w", err)
		}

		parts := strings.SplitN(header.Name, "/", 2)
		if len(parts) != 2 {
			continue
		}
		dirName, fileName := parts[0], parts[1]

		switch fileName {
		case "asset", "asset.meta":
			destPath := filepath.Join(tempDir, dirName, fileName)
			if err := os.MkdirAll(filepath.Dir(destPath), 0755); err != nil {
				return nil, fmt.Errorf("failed to create directory for %s: %w", destPath, err)
			}
			outFile, err := os.Create(destPath)
			if err != nil {
				return nil, fmt.Errorf("failed to create file %s: %w", destPath, err)
			}
			if _, err := io.Copy(outFile, tarReader); err != nil {
				outFile.Close()
				return nil, fmt.Errorf("failed to extract %s: %w", destPath, err)
			}
			outFile.Close()
			if fileName == "asset" {
				current.AssetPath = destPath
			} else {
				current.MetaPath = destPath
			}
		case "pathname":
			buf := new(strings.Builder)
			if _, err := io.Copy(buf, tarReader); err != nil {
				return nil, fmt.Errorf("failed to read pathname: %w", err)
			}
			current.PathName = buf.String()
			assets = append(assets, current)
			current = UnityAsset{} // reset
		}
	}
	fmt.Printf("[INFO] Extracted %d Unity assets\n", len(assets))
	return assets, nil
}

func moveFile(source, destination string) error {
	destDir := filepath.Dir(destination)
	if err := os.MkdirAll(destDir, 0755); err != nil {
		return fmt.Errorf("failed to create directory %s: %w", destDir, err)
	}

	// Try fast rename first
	err := os.Rename(source, destination)
	if err == nil {
		return nil
	}

	// Fallback to copy + delete
	in, err := os.Open(source)
	if err != nil {
		return fmt.Errorf("failed to open source file %s: %w", source, err)
	}
	defer in.Close()

	out, err := os.Create(destination)
	if err != nil {
		return fmt.Errorf("failed to create destination file %s: %w", destination, err)
	}
	defer out.Close()

	if _, err := io.Copy(out, in); err != nil {
		return fmt.Errorf("failed to copy data from %s to %s: %w", source, destination, err)
	}

	return nil
}

// reconstructStructure moves asset/meta files to their target locations.
func reconstructStructure(assets []UnityAsset, targetDir string) error {
	fmt.Printf("[INFO] Reconstructing Unity structure in: %s\n", targetDir)

	for _, asset := range assets {
		if asset.PathName == "" || asset.AssetPath == "" {
			fmt.Printf("[WARN] Skipping incomplete entry (missing pathname or asset)\n")
			continue
		}

		targetAsset := filepath.Join(targetDir, asset.PathName)
		targetMeta := targetAsset + ".meta"

		fmt.Printf("[INFO] Moving asset: %s\n", asset.PathName)

		if err := moveFile(asset.AssetPath, targetAsset); err != nil {
			return fmt.Errorf("failed to move asset: %w", err)
		}
		if asset.MetaPath != "" {
			if err := moveFile(asset.MetaPath, targetMeta); err != nil {
				return fmt.Errorf("failed to move meta: %w", err)
			}
		} else {
			fmt.Printf("[INFO] No .meta file found for %s\n", asset.PathName)
		}
	}
	fmt.Printf("[INFO] Reconstruction complete.\n")
	return nil
}

func main() {
	if len(os.Args) < 2 {
		fmt.Println("Usage: unpack <input_file> [output_folder]")
		os.Exit(1)
	}

	inputFile := os.Args[1]
	var outputDir string

	if len(os.Args) < 3 {
		baseName := filepath.Base(inputFile)
		outputDir = strings.TrimSuffix(baseName, filepath.Ext(baseName))
	} else {
		outputDir = os.Args[2]
	}

	fmt.Printf("[INFO] Input File: %s\n", inputFile)
	fmt.Printf("[INFO] Output Directory: %s\n", outputDir)

	if err := os.MkdirAll(outputDir, 0755); err != nil {
		fmt.Printf("[ERROR] Failed to create output directory: %v\n", err)
		os.Exit(1)
	}

	// Create temp dir on same drive as output
	tempParent := filepath.Join(outputDir, ".tmp_unpack")
	if err := os.MkdirAll(tempParent, os.ModePerm); err != nil {
		fmt.Printf("[ERROR] Failed to create temp directory root: %v\n", err)
		os.Exit(1)
	}
	tempDir, err := os.MkdirTemp(tempParent, "unpack-unitypackage-*")
	if err != nil {
		fmt.Printf("[ERROR] Failed to create temp directory: %v\n", err)
		os.Exit(1)
	}
	defer func() {
		if err := os.RemoveAll(tempParent); err != nil {
			fmt.Printf("[WARN] Failed to delete temp directory: %v\n", err)
		}
	}()

	// Step 1: Extract
	assets, err := extractTarGz(inputFile, tempDir)
	if err != nil {
		fmt.Printf("[ERROR] Extraction failed: %v\n", err)
		os.Exit(1)
	}

	// Step 2: Reconstruct
	if err := reconstructStructure(assets, outputDir); err != nil {
		fmt.Printf("[ERROR] Reconstruction failed: %v\n", err)
		os.Exit(1)
	}

	fmt.Printf("[SUCCESS] Unpacked %s into %s\n", inputFile, outputDir)
}
